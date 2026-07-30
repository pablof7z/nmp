//! Print one named UniFFI component interface embedded in a native library.
//!
//! This standalone governance tool is deliberately outside the NMP workspace
//! and product crate. Its source, manifest, and lockfile are trusted from the
//! PR base. The output comes from proc-macro metadata in library mode, not UDL.

use std::{collections::BTreeSet, env, fs, io};

use uniffi_bindgen::{library_mode, EmptyCrateConfigSupplier};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 5 {
        return Err(io::Error::other(
            "usage: nmp-component-interface-snapshot <library> <component-key> \
             <uniffi-namespace> <allowed-namespaces-nul-file>",
        )
        .into());
    }
    let library = &args[1];
    let component_key = &args[2];
    let namespace = &args[3];
    let allowed_namespaces = read_allowed_namespaces(&args[4])?;

    let components =
        library_mode::find_components(library.as_str().into(), &EmptyCrateConfigSupplier)?;
    let component_namespaces = components
        .iter()
        .map(|component| component.ci.namespace().to_owned())
        .collect::<Vec<_>>();
    let selected = select_component_namespace(
        &component_namespaces,
        component_key,
        namespace,
        &allowed_namespaces,
    )?;
    let component = &components[selected];
    let ci = &component.ci;

    println!("# NMP UniFFI component interface");
    println!("# source: proc-macro metadata extracted in library mode (not UDL)");
    println!("# uniffi: 0.29.5");
    println!("component {:?}", component_key);
    println!("crate {:?}", ci.crate_name());
    println!("namespace {:?}", ci.namespace());

    let mut enums = ci.enum_definitions().collect::<Vec<_>>();
    enums.sort_by_key(|definition| definition.name());
    for definition in enums {
        println!("\nenum {:#?}", definition);
    }

    let mut records = ci.record_definitions().collect::<Vec<_>>();
    records.sort_by_key(|definition| definition.name());
    for definition in records {
        println!("\nrecord {:#?}", definition);
    }

    let mut functions = ci.function_definitions().iter().collect::<Vec<_>>();
    functions.sort_by_key(|definition| definition.name());
    for definition in functions {
        println!("\nfunction {:#?}", definition);
    }

    let mut objects = ci.object_definitions().iter().collect::<Vec<_>>();
    objects.sort_by_key(|definition| definition.name());
    for definition in objects {
        println!("\nobject {:#?}", definition);
    }

    let mut callbacks = ci
        .callback_interface_definitions()
        .iter()
        .collect::<Vec<_>>();
    callbacks.sort_by_key(|definition| definition.name());
    for definition in callbacks {
        println!("\ncallback {:#?}", definition);
    }

    Ok(())
}

fn read_allowed_namespaces(path: &str) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() || bytes.last() != Some(&0) {
        return Err(io::Error::other(
            "allowed namespace file must be non-empty and NUL-terminated",
        )
        .into());
    }
    let mut namespaces = BTreeSet::new();
    for field in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        let namespace = std::str::from_utf8(field)
            .map_err(|_| io::Error::other("allowed namespace file is not UTF-8"))?;
        if namespace.is_empty() || !namespaces.insert(namespace.to_owned()) {
            return Err(io::Error::other(format!(
                "allowed namespace file contains an empty or duplicate entry: {namespace:?}"
            ))
            .into());
        }
    }
    Ok(namespaces)
}

fn select_component_namespace(
    component_namespaces: &[String],
    component_key: &str,
    namespace: &str,
    allowed_namespaces: &BTreeSet<String>,
) -> Result<usize, Box<dyn std::error::Error>> {
    if !allowed_namespaces.contains(namespace) {
        return Err(io::Error::other(format!(
            "component {component_key} requests namespace {namespace:?}, which is absent from the active catalog"
        ))
        .into());
    }
    let undeclared = component_namespaces
        .iter()
        .filter(|found| !allowed_namespaces.contains(found.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    if !undeclared.is_empty() {
        return Err(io::Error::other(format!(
            "component library contains undeclared UniFFI namespace(s): {undeclared:?}"
        ))
        .into());
    }
    let matches = component_namespaces
        .iter()
        .enumerate()
        .filter(|(_, found)| found.as_str() == namespace)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(io::Error::other(format!(
            "expected exactly one UniFFI component namespace {namespace:?} for {component_key}; found {}",
            matches.len()
        ))
        .into());
    }
    Ok(matches[0])
}

#[cfg(test)]
mod tests {
    use super::select_component_namespace;
    use std::collections::BTreeSet;

    fn allowed(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn selects_one_declared_namespace_from_a_multi_namespace_library() {
        let namespaces = vec!["core_ffi".to_owned(), "provider_ffi".to_owned()];
        assert_eq!(
            select_component_namespace(
                &namespaces,
                "provider",
                "provider_ffi",
                &allowed(&["core_ffi", "provider_ffi"])
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn missing_requested_namespace_is_refused() {
        let namespaces = vec!["core_ffi".to_owned()];
        assert!(select_component_namespace(
            &namespaces,
            "provider",
            "provider_ffi",
            &allowed(&["core_ffi", "provider_ffi"])
        )
        .is_err());
    }

    #[test]
    fn undeclared_extra_namespace_is_refused() {
        let namespaces = vec!["provider_ffi".to_owned(), "impostor_ffi".to_owned()];
        assert!(select_component_namespace(
            &namespaces,
            "provider",
            "provider_ffi",
            &allowed(&["provider_ffi"])
        )
        .is_err());
    }

    #[test]
    fn wrong_catalog_namespace_is_refused() {
        let namespaces = vec!["provider_ffi".to_owned()];
        assert!(select_component_namespace(
            &namespaces,
            "provider",
            "wrong_ffi",
            &allowed(&["provider_ffi"])
        )
        .is_err());
    }

    #[test]
    fn duplicate_namespace_metadata_is_refused() {
        let namespaces = vec!["provider_ffi".to_owned(), "provider_ffi".to_owned()];
        assert!(select_component_namespace(
            &namespaces,
            "provider",
            "provider_ffi",
            &allowed(&["provider_ffi"])
        )
        .is_err());
    }
}
