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
const CORE_MODULE: &str = "nmp_ffi";
const MAILBOX_TYPE: &str = "FfiSignerMailbox";
const COMPATIBILITY_TYPE: &str = "FfiNip46CoreCompatibility";
const CORE_MAILBOX_SOURCE: &str = "nmp_ffi::NmpEngine::signer_mailbox";
const REQUIRED_ENTRY: &str = "nmp_nip46_ffi::NmpNip46Provider::new";

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
    let has_mailbox_metadata = items.iter().any(|item| {
        matches!(
            item,
            Metadata::Object(object)
                if object.module_path == CORE_MODULE && object.name == MAILBOX_TYPE
        )
    });
    if !has_mailbox_metadata {
        return Err(format!(
            "compiled metadata has no {CORE_MODULE}::{MAILBOX_TYPE} positive control"
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

    let mut core_mailbox_sources = 0;
    let mut mailbox_entries = Vec::new();
    for callable in library_callables(items) {
        let input_carries_mailbox = callable
            .inputs
            .iter()
            .any(|ty| graph.contains_named(ty, CORE_MODULE, MAILBOX_TYPE));
        let return_carries_mailbox = callable
            .return_type
            .is_some_and(|ty| graph.contains_named(ty, CORE_MODULE, MAILBOX_TYPE));
        let throws_carries_mailbox = callable
            .throws
            .is_some_and(|ty| graph.contains_named(ty, CORE_MODULE, MAILBOX_TYPE));
        let carries_mailbox =
            input_carries_mailbox || return_carries_mailbox || throws_carries_mailbox;
        if !carries_mailbox {
            continue;
        }

        // The core vends the opaque mailbox through one exact outward-only
        // callable. Do not exempt its namespace: a linked crate may choose
        // the same `[lib] name` and therefore claim the same UniFFI module.
        // UniFFI derives both this label and its no-mangle symbols from the
        // same module/type/function tuple, so forging the exact allowance
        // collides at link time; the exact-one check below remains the second
        // line of defence. An input, throws position, duplicate, or any other
        // callable remains subject to the provider-proof rule below.
        if callable.label == CORE_MAILBOX_SOURCE
            && return_carries_mailbox
            && !input_carries_mailbox
            && !throws_carries_mailbox
        {
            core_mailbox_sources += 1;
            continue;
        }

        let requires_compatibility = callable
            .inputs
            .iter()
            .any(|ty| graph.contains_named(ty, PROVIDER_MODULE, COMPATIBILITY_TYPE));
        if !requires_compatibility {
            return Err(format!(
                "compiled UniFFI entry {} carries {CORE_MODULE}::{MAILBOX_TYPE} without an input containing {PROVIDER_MODULE}::{COMPATIBILITY_TYPE}",
                callable.label
            ));
        }
        mailbox_entries.push(callable.label);
    }

    if core_mailbox_sources != 1 {
        return Err(format!(
            "compiled UniFFI metadata must expose exactly one outward-only core mailbox source ({CORE_MAILBOX_SOURCE}); found {core_mailbox_sources}"
        ));
    }

    if mailbox_entries != [REQUIRED_ENTRY] {
        return Err(format!(
            "compiled UniFFI metadata must expose exactly one proof-bearing mailbox entry ({REQUIRED_ENTRY}); found {mailbox_entries:?}"
        ));
    }

    Ok(format!(
        "nip46-provider-metadata: one compiled mailbox entry, proof-bearing: {REQUIRED_ENTRY}"
    ))
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
        ConstructorMetadata, FieldMetadata, FnMetadata, FnParamMetadata, MethodMetadata,
        ObjectImpl, ObjectMetadata, RecordMetadata,
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
        vec![
            object(CORE_MODULE, MAILBOX_TYPE),
            object(PROVIDER_MODULE, COMPATIBILITY_TYPE),
            Metadata::Constructor(ConstructorMetadata {
                module_path: PROVIDER_MODULE.to_string(),
                self_name: "NmpNip46Provider".to_string(),
                name: "new".to_string(),
                is_async: false,
                inputs: vec![
                    FnParamMetadata::simple(
                        "compatibility",
                        object_type(PROVIDER_MODULE, COMPATIBILITY_TYPE),
                    ),
                    FnParamMetadata::simple("mailbox", object_type(CORE_MODULE, MAILBOX_TYPE)),
                ],
                throws: None,
                checksum: None,
                docstring: None,
            }),
            Metadata::Method(MethodMetadata {
                module_path: CORE_MODULE.to_string(),
                self_name: "NmpEngine".to_string(),
                name: "signer_mailbox".to_string(),
                is_async: false,
                inputs: Vec::new(),
                return_type: Some(object_type(CORE_MODULE, MAILBOX_TYPE)),
                throws: None,
                takes_self_by_arc: true,
                checksum: None,
                docstring: None,
            }),
        ]
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

    fn provider_function(name: &str, input: Type, return_type: Option<Type>) -> Metadata {
        function_in_module(PROVIDER_MODULE, name, vec![input], return_type)
    }

    fn proof_bearing_mailbox_function(module_path: &str, name: &str) -> Metadata {
        function_in_module(
            module_path,
            name,
            vec![
                object_type(PROVIDER_MODULE, COMPATIBILITY_TYPE),
                object_type(CORE_MODULE, MAILBOX_TYPE),
            ],
            None,
        )
    }

    #[test]
    fn compiled_record_carrier_cannot_hide_an_unproven_mailbox_entry() {
        let mut items = valid_metadata();
        items.push(Metadata::Record(RecordMetadata {
            module_path: PROVIDER_MODULE.to_string(),
            name: "MailboxCarrier".to_string(),
            remote: false,
            fields: vec![FieldMetadata {
                name: "inner".to_string(),
                ty: Type::Optional {
                    inner_type: Box::new(object_type(CORE_MODULE, MAILBOX_TYPE)),
                },
                default: None,
                docstring: None,
            }],
            docstring: None,
        }));
        items.push(provider_function(
            "smuggled_mailbox",
            Type::Record {
                module_path: PROVIDER_MODULE.to_string(),
                name: "MailboxCarrier".to_string(),
            },
            None,
        ));

        let error = audit(&items).expect_err("the record carrier must be resolved recursively");
        assert!(error.contains("smuggled_mailbox"));
        assert!(error.contains("without an input containing"));
    }

    #[test]
    fn compiled_return_position_is_part_of_the_mailbox_boundary() {
        let mut items = valid_metadata();
        items.push(provider_function(
            "returns_mailbox",
            Type::String,
            Some(object_type(CORE_MODULE, MAILBOX_TYPE)),
        ));

        let error = audit(&items).expect_err("return-position mailbox exports must not be hidden");
        assert!(error.contains("returns_mailbox"));
    }

    #[test]
    fn exact_compiled_constructor_is_the_only_mailbox_entry() {
        let mut items = valid_metadata();
        items.push(proof_bearing_mailbox_function(
            PROVIDER_MODULE,
            "second_mailbox_entry",
        ));

        let error = audit(&items).expect_err("a second proof-bearing mailbox entry must fail");
        assert!(error.contains("must expose exactly one"));
        assert!(error.contains("second_mailbox_entry"));
    }

    #[test]
    fn missing_compiled_mailbox_entry_is_rejected() {
        let mut items = valid_metadata();
        items.retain(|item| !matches!(item, Metadata::Constructor(_)));

        let error = audit(&items).expect_err("zero mailbox entries must fail");
        assert!(error.contains("found []"));
    }

    #[test]
    fn exact_compiled_constructor_name_is_required() {
        let mut items = valid_metadata();
        let Some(Metadata::Constructor(constructor)) = items
            .iter_mut()
            .find(|item| matches!(item, Metadata::Constructor(_)))
        else {
            panic!("valid metadata must contain the provider constructor");
        };
        constructor.name = "create".to_string();

        let error = audit(&items).expect_err("renaming the required constructor must fail");
        assert!(error.contains(REQUIRED_ENTRY));
        assert!(error.contains("NmpNip46Provider::create"));
    }

    #[test]
    fn missing_mailbox_metadata_positive_control_is_rejected() {
        let mut items = valid_metadata();
        items.retain(|item| {
            !matches!(
                item,
                Metadata::Object(object)
                    if object.module_path == CORE_MODULE && object.name == MAILBOX_TYPE
            )
        });

        let error = audit(&items).expect_err("missing mailbox metadata must fail closed");
        assert!(error.contains("FfiSignerMailbox positive control"));
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
        assert!(error.contains("FfiNip46CoreCompatibility positive control"));
    }

    #[test]
    fn foreign_namespace_mailbox_entry_is_not_hidden_from_audit() {
        let mut items = valid_metadata();
        items.push(function_in_module(
            "nmp_nip46_sideload",
            "sideload_take_mailbox",
            vec![object_type(CORE_MODULE, MAILBOX_TYPE)],
            None,
        ));

        let error = audit(&items).expect_err("linked non-core namespaces must be audited");
        assert!(error.contains("nmp_nip46_sideload::sideload_take_mailbox"));
        assert!(error.contains("without an input containing"));
    }

    #[test]
    fn forged_core_namespace_mailbox_input_is_not_exempted() {
        let mut items = valid_metadata();
        items.push(function_in_module(
            CORE_MODULE,
            "impostor_take_mailbox",
            vec![object_type(CORE_MODULE, MAILBOX_TYPE)],
            None,
        ));

        let error =
            audit(&items).expect_err("claiming the core namespace must not exempt mailbox inputs");
        assert!(error.contains("nmp_ffi::impostor_take_mailbox"));
        assert!(error.contains("without an input containing"));
    }

    #[test]
    fn exact_core_mailbox_source_cannot_accept_a_mailbox_input() {
        let mut items = valid_metadata();
        let Some(Metadata::Method(method)) = items
            .iter_mut()
            .find(|item| matches!(item, Metadata::Method(_)))
        else {
            panic!("valid metadata must contain the core mailbox source");
        };
        method.inputs.push(FnParamMetadata::simple(
            "mailbox",
            object_type(CORE_MODULE, MAILBOX_TYPE),
        ));

        let error =
            audit(&items).expect_err("the exact core source is exempt only in return position");
        assert!(error.contains(CORE_MAILBOX_SOURCE));
        assert!(error.contains("without an input containing"));
    }

    #[test]
    fn exact_core_mailbox_source_cannot_throw_a_mailbox() {
        let mut items = valid_metadata();
        let Some(Metadata::Method(method)) = items
            .iter_mut()
            .find(|item| matches!(item, Metadata::Method(_)))
        else {
            panic!("valid metadata must contain the core mailbox source");
        };
        method.throws = Some(object_type(CORE_MODULE, MAILBOX_TYPE));

        let error =
            audit(&items).expect_err("the exact core source is exempt only in return position");
        assert!(error.contains(CORE_MAILBOX_SOURCE));
        assert!(error.contains("without an input containing"));
    }

    #[test]
    fn duplicate_core_mailbox_source_is_rejected() {
        let mut items = valid_metadata();
        let duplicate = items
            .iter()
            .find_map(|item| match item {
                Metadata::Method(method) => Some(method.clone()),
                _ => None,
            })
            .expect("valid metadata must contain the core mailbox source");
        items.push(Metadata::Method(duplicate));

        let error = audit(&items).expect_err("two core mailbox sources must fail");
        assert!(error.contains("exactly one outward-only core mailbox source"));
        assert!(error.contains("found 2"));
    }

    #[test]
    fn missing_core_mailbox_source_positive_control_is_rejected() {
        let mut items = valid_metadata();
        items.retain(|item| !matches!(item, Metadata::Method(_)));

        let error = audit(&items).expect_err("the outward core mailbox source must be present");
        assert!(error.contains("exactly one outward-only core mailbox source"));
        assert!(error.contains(CORE_MAILBOX_SOURCE));
    }

    #[test]
    fn valid_compiled_constructor_passes_the_full_audit() {
        assert!(audit(&valid_metadata()).is_ok());
    }
}
