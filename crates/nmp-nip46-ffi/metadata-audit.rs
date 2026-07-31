//! Audit the provider's compiled UniFFI metadata before an SDK packages it.
//!
//! Source parsing cannot enumerate a macro-expanded API, an out-of-tree
//! `#[path]` module, or every Rust spelling that UniFFI accepts. The metadata
//! embedded in the native library is the binding generator's own authority,
//! so this tool checks that representation instead.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::PathBuf,
};

use camino::Utf8PathBuf;
use uniffi_meta::{Metadata, Type};

const PROVIDER_MODULE: &str = "nmp_nip46_ffi";
const INTERFACE_MODULE: &str = "nmp_component_interface";
const ADAPTER_TYPE: &str = "FfiSignerAdapter";
const COMPATIBILITY_TYPE: &str = "FfiNip46Compatibility";
const PREPARED_TYPE: &str = "FfiNip46PreparedConnection";
const PROOF_ENTRY: &str = "nmp_nip46_ffi::verify_nip46_component";
const ADAPTER_ENTRY: &str = "nmp_nip46_ffi::FfiNip46PreparedConnection::adapter";
const REQUIRED_ADAPTER_ENTRIES: &[&str] = &[
    "nmp_nip46_ffi::prepare_nip46_bunker",
    "nmp_nip46_ffi::prepare_nip46_invitation",
    "nmp_nip46_ffi::prepare_nip46_restore",
];
const FORBIDDEN_CORE_TYPES: &[&str] = &[
    "FfiSignerMailbox",
    "FfiSignerRegistration",
    "CoreSignerPort",
    "CoreSignerLease",
    "CoreDetach",
    "NmpEngine",
    "FfiSignerAdapterInstallation",
];

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TypeKey {
    module_path: String,
    name: String,
}

impl TypeKey {
    fn new(module_path: &str, name: &str) -> Self {
        Self {
            module_path: module_path.to_string(),
            name: name.to_string(),
        }
    }
}

#[derive(Default)]
struct TypeGraph {
    definitions: BTreeMap<TypeKey, Vec<Type>>,
}

impl TypeGraph {
    fn from_metadata(items: &[Metadata]) -> Self {
        let mut graph = Self::default();
        for item in items {
            match item {
                Metadata::Record(record) => {
                    graph.definitions.insert(
                        TypeKey::new(&record.module_path, &record.name),
                        record.fields.iter().map(|field| field.ty.clone()).collect(),
                    );
                }
                Metadata::Enum(enumeration) => {
                    graph.definitions.insert(
                        TypeKey::new(&enumeration.module_path, &enumeration.name),
                        enumeration
                            .variants
                            .iter()
                            .flat_map(|variant| variant.fields.iter().map(|field| field.ty.clone()))
                            .collect(),
                    );
                }
                Metadata::CustomType(custom) => {
                    graph.definitions.insert(
                        TypeKey::new(&custom.module_path, &custom.name),
                        vec![custom.builtin.clone()],
                    );
                }
                _ => {}
            }
        }
        graph
    }

    fn contains_named(&self, ty: &Type, module_path: &str, name: &str) -> bool {
        self.contains_named_inner(ty, module_path, name, &mut BTreeSet::new())
    }

    fn contains_named_inner(
        &self,
        ty: &Type,
        module_path: &str,
        name: &str,
        visiting: &mut BTreeSet<TypeKey>,
    ) -> bool {
        if ty.name() == Some(name) && ty.module_path() == Some(module_path) {
            return true;
        }

        match ty {
            Type::Optional { inner_type } | Type::Sequence { inner_type } => {
                self.contains_named_inner(inner_type, module_path, name, visiting)
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                self.contains_named_inner(key_type, module_path, name, visiting)
                    || self.contains_named_inner(value_type, module_path, name, visiting)
            }
            Type::Record {
                module_path: definition_module,
                name: definition_name,
            }
            | Type::Enum {
                module_path: definition_module,
                name: definition_name,
            }
            | Type::Custom {
                module_path: definition_module,
                name: definition_name,
                ..
            } => {
                let key = TypeKey::new(definition_module, definition_name);
                if !visiting.insert(key.clone()) {
                    return false;
                }
                let found = self.definitions.get(&key).is_some_and(|fields| {
                    fields
                        .iter()
                        .any(|field| self.contains_named_inner(field, module_path, name, visiting))
                });
                visiting.remove(&key);
                found
            }
            _ => false,
        }
    }
}

struct Callable<'a> {
    label: String,
    inputs: Vec<&'a Type>,
    return_type: Option<&'a Type>,
    throws: Option<&'a Type>,
}

fn library_callables(items: &[Metadata]) -> Vec<Callable<'_>> {
    let mut callables = Vec::new();
    for item in items {
        let (label, inputs, return_type, throws) = match item {
            Metadata::Func(function) => (
                format!("{}::{}", function.module_path, function.name),
                function
                    .inputs
                    .iter()
                    .map(|input| &input.ty)
                    .collect::<Vec<_>>(),
                function.return_type.as_ref(),
                function.throws.as_ref(),
            ),
            Metadata::Constructor(constructor) => (
                format!(
                    "{}::{}::{}",
                    constructor.module_path, constructor.self_name, constructor.name
                ),
                constructor
                    .inputs
                    .iter()
                    .map(|input| &input.ty)
                    .collect::<Vec<_>>(),
                None,
                constructor.throws.as_ref(),
            ),
            Metadata::Method(method) => (
                format!(
                    "{}::{}::{}",
                    method.module_path, method.self_name, method.name
                ),
                method
                    .inputs
                    .iter()
                    .map(|input| &input.ty)
                    .collect::<Vec<_>>(),
                method.return_type.as_ref(),
                method.throws.as_ref(),
            ),
            Metadata::TraitMethod(method) => (
                format!(
                    "{}::{}::{}",
                    method.module_path, method.trait_name, method.name
                ),
                method
                    .inputs
                    .iter()
                    .map(|input| &input.ty)
                    .collect::<Vec<_>>(),
                method.return_type.as_ref(),
                method.throws.as_ref(),
            ),
            _ => continue,
        };

        callables.push(Callable {
            label,
            inputs,
            return_type,
            throws,
        });
    }
    callables
}

fn audit(items: &[Metadata]) -> Result<String, String> {
    let graph = TypeGraph::from_metadata(items);
    let has_adapter_metadata = items.iter().any(|item| {
        matches!(
            item,
            Metadata::Object(object)
                if object.module_path == INTERFACE_MODULE && object.name == ADAPTER_TYPE
        )
    });
    if !has_adapter_metadata {
        return Err(format!(
            "compiled metadata has no {INTERFACE_MODULE}::{ADAPTER_TYPE} positive control"
        ));
    }
    let has_compatibility_metadata = items.iter().any(|item| {
        matches!(
            item,
            Metadata::Object(object)
                if object.module_path == PROVIDER_MODULE
                    && object.name == COMPATIBILITY_TYPE
        )
    });
    if !has_compatibility_metadata {
        return Err(format!(
            "compiled metadata has no {PROVIDER_MODULE}::{COMPATIBILITY_TYPE} positive control"
        ));
    }
    if items.iter().any(|item| {
        matches!(
            item,
            Metadata::Constructor(constructor)
                if constructor.module_path == PROVIDER_MODULE
                    && constructor.self_name == PREPARED_TYPE
        )
    }) {
        return Err(format!(
            "compiled metadata exposes a forbidden {PREPARED_TYPE} constructor"
        ));
    }
    for item in items {
        let (module, name) = match item {
            Metadata::Object(value) => (&value.module_path, &value.name),
            Metadata::Record(value) => (&value.module_path, &value.name),
            Metadata::Enum(value) => (&value.module_path, &value.name),
            _ => continue,
        };
        if module == "nmp_ffi" || FORBIDDEN_CORE_TYPES.contains(&name.as_str()) {
            return Err(format!(
                "compiled provider metadata contains forbidden core authority {module}::{name}"
            ));
        }
    }

    let mut adapter_entries = BTreeSet::new();
    let mut preparation_entries = BTreeSet::new();
    let mut proof_entries = BTreeSet::new();
    for callable in library_callables(items) {
        let adapter_input = callable
            .inputs
            .iter()
            .any(|ty| graph.contains_named(ty, INTERFACE_MODULE, ADAPTER_TYPE));
        let adapter_return = callable
            .return_type
            .is_some_and(|ty| graph.contains_named(ty, INTERFACE_MODULE, ADAPTER_TYPE));
        let adapter_throw = callable
            .throws
            .is_some_and(|ty| graph.contains_named(ty, INTERFACE_MODULE, ADAPTER_TYPE));
        let compatibility_inputs = callable
            .inputs
            .iter()
            .enumerate()
            .filter_map(|(index, ty)| {
                graph
                    .contains_named(ty, PROVIDER_MODULE, COMPATIBILITY_TYPE)
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        let compatibility_return = callable
            .return_type
            .is_some_and(|ty| graph.contains_named(ty, PROVIDER_MODULE, COMPATIBILITY_TYPE));
        let prepared_return = callable
            .return_type
            .is_some_and(|ty| graph.contains_named(ty, PROVIDER_MODULE, PREPARED_TYPE));
        if compatibility_return && compatibility_inputs.is_empty() {
            proof_entries.insert(callable.label.clone());
        }
        if prepared_return {
            if compatibility_inputs != [0] {
                return Err(format!(
                    "compiled UniFFI entry {} must receive compatibility proof at input zero before returning {PREPARED_TYPE}",
                    callable.label
                ));
            }
            preparation_entries.insert(callable.label.clone());
        }
        if !(adapter_input || adapter_return || adapter_throw) {
            continue;
        }
        if adapter_input || adapter_throw || !adapter_return {
            return Err(format!(
                "compiled UniFFI entry {} must return the provider adapter, never receive or throw it",
                callable.label
            ));
        }
        if compatibility_inputs != [0] {
            return Err(format!(
                "compiled UniFFI entry {} must receive exactly one compatibility proof at input zero before returning an adapter",
                callable.label
            ));
        }
        adapter_entries.insert(callable.label);
    }

    let expected_preparations = REQUIRED_ADAPTER_ENTRIES
        .iter()
        .map(|entry| (*entry).to_string())
        .collect::<BTreeSet<_>>();
    if preparation_entries != expected_preparations {
        return Err(format!(
            "compiled UniFFI metadata has wrong proof-first preparation set; found {preparation_entries:?}"
        ));
    }
    let expected_adapters = BTreeSet::from([ADAPTER_ENTRY.to_string()]);
    if adapter_entries != expected_adapters {
        return Err(format!(
            "compiled UniFFI metadata must expose exactly one proof-bearing adapter accessor ({ADAPTER_ENTRY}); found {adapter_entries:?}"
        ));
    }
    let expected_proof = BTreeSet::from([PROOF_ENTRY.to_string()]);
    if proof_entries != expected_proof {
        return Err(format!(
            "compiled UniFFI metadata must expose exactly one compatibility proof constructor ({PROOF_ENTRY}); found {proof_entries:?}"
        ));
    }

    Ok("nip46-provider-metadata: four proof-first adapter preparations, one proof constructor, zero core authority types".to_string())
}

fn run() -> Result<(), String> {
    let mut arguments = env::args_os();
    let executable = arguments.next().unwrap_or_default();
    let Some(library) = arguments.next() else {
        return Err(format!(
            "usage: {} LIBRARY",
            PathBuf::from(executable).display()
        ));
    };
    if arguments.next().is_some() {
        return Err(format!(
            "usage: {} LIBRARY",
            PathBuf::from(executable).display()
        ));
    }
    let library = Utf8PathBuf::from_path_buf(PathBuf::from(library))
        .map_err(|path| format!("library path is not valid UTF-8: {}", path.display()))?;
    let metadata = uniffi_bindgen::macro_metadata::extract_from_library(&library)
        .map_err(|error| format!("extract UniFFI metadata from {library}: {error:#}"))?;
    println!("{}", audit(&metadata)?);
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("nip46-provider-metadata: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniffi_meta::{
        ConstructorMetadata, FnMetadata, FnParamMetadata, MethodMetadata, ObjectImpl,
        ObjectMetadata,
    };

    fn object(module_path: &str, name: &str) -> Metadata {
        Metadata::Object(ObjectMetadata {
            module_path: module_path.to_string(),
            name: name.to_string(),
            remote: false,
            imp: ObjectImpl::Struct,
            docstring: None,
        })
    }

    fn object_type(module_path: &str, name: &str) -> Type {
        Type::Object {
            module_path: module_path.to_string(),
            name: name.to_string(),
            imp: ObjectImpl::Struct,
        }
    }

    fn valid_metadata() -> Vec<Metadata> {
        let mut items = vec![
            object(INTERFACE_MODULE, ADAPTER_TYPE),
            object(PROVIDER_MODULE, COMPATIBILITY_TYPE),
            object(PROVIDER_MODULE, PREPARED_TYPE),
            Metadata::Method(MethodMetadata {
                module_path: PROVIDER_MODULE.to_string(),
                self_name: PREPARED_TYPE.to_string(),
                name: "adapter".to_string(),
                is_async: false,
                inputs: vec![FnParamMetadata::simple(
                    "compatibility",
                    object_type(PROVIDER_MODULE, COMPATIBILITY_TYPE),
                )],
                return_type: Some(object_type(INTERFACE_MODULE, ADAPTER_TYPE)),
                throws: None,
                takes_self_by_arc: false,
                checksum: None,
                docstring: None,
            }),
            function_in_module(
                PROVIDER_MODULE,
                "verify_nip46_component",
                vec![Type::String, Type::String, Type::String, Type::String],
                Some(object_type(PROVIDER_MODULE, COMPATIBILITY_TYPE)),
            ),
        ];
        for entry in REQUIRED_ADAPTER_ENTRIES {
            items.push(function_in_module(
                PROVIDER_MODULE,
                entry.rsplit("::").next().unwrap(),
                vec![
                    object_type(PROVIDER_MODULE, COMPATIBILITY_TYPE),
                    Type::String,
                ],
                Some(object_type(PROVIDER_MODULE, PREPARED_TYPE)),
            ));
        }
        items
    }

    fn function_in_module(
        module_path: &str,
        name: &str,
        inputs: Vec<Type>,
        return_type: Option<Type>,
    ) -> Metadata {
        Metadata::Func(FnMetadata {
            module_path: module_path.to_string(),
            name: name.to_string(),
            is_async: false,
            inputs: inputs
                .into_iter()
                .enumerate()
                .map(|(index, input)| FnParamMetadata::simple(&format!("value_{index}"), input))
                .collect(),
            return_type,
            throws: None,
            checksum: None,
            docstring: None,
        })
    }

    #[test]
    fn missing_adapter_metadata_positive_control_is_rejected() {
        let mut items = valid_metadata();
        items.retain(|item| {
            !matches!(
                item,
                Metadata::Object(object)
                    if object.module_path == INTERFACE_MODULE && object.name == ADAPTER_TYPE
            )
        });

        let error = audit(&items).expect_err("missing adapter metadata must fail closed");
        assert!(error.contains("FfiSignerAdapter positive control"));
    }

    #[test]
    fn missing_compatibility_metadata_positive_control_is_rejected() {
        let mut items = valid_metadata();
        items.retain(|item| {
            !matches!(
                item,
                Metadata::Object(object)
                    if object.module_path == PROVIDER_MODULE
                        && object.name == COMPATIBILITY_TYPE
            )
        });

        let error = audit(&items).expect_err("missing compatibility metadata must fail closed");
        assert!(error.contains("FfiNip46Compatibility positive control"));
    }

    #[test]
    fn adapter_return_requires_proof_at_input_zero() {
        let mut items = valid_metadata();
        let Some(Metadata::Func(function)) = items
            .iter_mut()
            .find(|item| matches!(item, Metadata::Func(function) if function.name == "prepare_nip46_bunker"))
        else {
            panic!("valid metadata must contain a prepare function");
        };
        function.inputs.swap(0, 1);

        let error = audit(&items).expect_err("proof moved from input zero must fail");
        assert!(error.contains("input zero"));
    }

    #[test]
    fn adapter_input_is_refused() {
        let mut items = valid_metadata();
        items.push(function_in_module(
            PROVIDER_MODULE,
            "take_adapter",
            vec![object_type(INTERFACE_MODULE, ADAPTER_TYPE)],
            None,
        ));
        let error = audit(&items).expect_err("provider must never receive an adapter");
        assert!(error.contains("never receive or throw"));
    }

    #[test]
    fn core_authority_type_is_refused() {
        let mut items = valid_metadata();
        items.push(object("nmp_ffi", "NmpEngine"));
        let error = audit(&items).expect_err("provider metadata must contain no core type");
        assert!(error.contains("forbidden core authority"));
    }

    #[test]
    fn prepared_connection_constructor_is_refused() {
        let mut items = valid_metadata();
        items.push(Metadata::Constructor(ConstructorMetadata {
            module_path: PROVIDER_MODULE.to_string(),
            self_name: PREPARED_TYPE.to_string(),
            name: "new".to_string(),
            is_async: false,
            inputs: vec![],
            throws: None,
            checksum: None,
            docstring: None,
        }));
        let error = audit(&items).expect_err("prepared carrier must be constructorless");
        assert!(error.contains("forbidden FfiNip46PreparedConnection constructor"));
    }

    #[test]
    fn duplicate_proof_constructor_is_refused() {
        let mut items = valid_metadata();
        items.push(function_in_module(
            PROVIDER_MODULE,
            "forge_compatibility",
            vec![],
            Some(object_type(PROVIDER_MODULE, COMPATIBILITY_TYPE)),
        ));
        let error = audit(&items).expect_err("a second proof constructor must fail");
        assert!(error.contains("forge_compatibility"));
    }

    #[test]
    fn exact_compiled_surface_passes_the_full_audit() {
        assert!(audit(&valid_metadata()).is_ok());
    }
}
