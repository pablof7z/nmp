//! Bindgen entry point for the optional NIP-46 UniFFI component.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use uniffi_bindgen::bindings::KotlinBindingGenerator;
use uniffi_bindgen::{
    BindgenCrateConfigSupplier, BindingGenerator, Component, ComponentInterface,
    EmptyCrateConfigSupplier, GenerationSettings,
};
use uniffi_meta::{create_metadata_groups, group_metadata, NamespaceMetadata};

fn merged_kotlin(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut core = None;
    let mut provider = None;
    let mut out = None;
    let mut index = 0;
    while index < arguments.len() {
        let destination = match arguments[index].as_str() {
            "--core-library" => &mut core,
            "--provider-library" => &mut provider,
            "--out-dir" => &mut out,
            unknown => return Err(format!("unknown merged Kotlin option {unknown:?}").into()),
        };
        index += 1;
        let value = arguments
            .get(index)
            .ok_or_else(|| "merged Kotlin option has no value".to_string())?;
        *destination = Some(Utf8PathBuf::from(value));
        index += 1;
    }
    let core = core.ok_or("missing --core-library")?;
    let provider = provider.ok_or("missing --provider-library")?;
    let out = out.ok_or("missing --out-dir")?;

    let mut items = uniffi_bindgen::macro_metadata::extract_from_library(&provider)?;
    for item in uniffi_bindgen::macro_metadata::extract_from_library(&core)? {
        if !items.contains(&item) {
            items.push(item);
        }
    }
    let mut groups = create_metadata_groups(&items);
    group_metadata(&mut groups, items)?;
    let namespaces: BTreeMap<String, NamespaceMetadata> = groups
        .iter()
        .map(|(crate_name, group)| (crate_name.clone(), group.namespace.clone()))
        .collect();
    let supplier = EmptyCrateConfigSupplier;
    let generator = KotlinBindingGenerator;
    let mut components = Vec::new();
    for group in groups.into_values() {
        let mut interface = ComponentInterface::new(&group.namespace.crate_name);
        interface.add_metadata(group)?;
        interface.set_crate_to_namespace_map(namespaces.clone());
        let crate_config = supplier
            .get_toml(interface.crate_name())?
            .unwrap_or_default();
        let config = generator.new_config(&toml05::Value::Table(crate_config))?;
        components.push(Component {
            ci: interface,
            config,
        });
    }
    let settings = GenerationSettings {
        out_dir: out,
        try_format_code: true,
        cdylib: Some("nmp_nip46_ffi".to_string()),
    };
    generator.update_component_configs(&settings, &mut components)?;
    components.retain(|component| component.ci.crate_name() == "nmp_nip46_ffi");
    if components.len() != 1 {
        return Err(format!(
            "merged metadata produced {} nmp_nip46_ffi components",
            components.len()
        )
        .into());
    }
    generator.write_bindings(&settings, &components)?;
    Ok(())
}

fn main() {
    let arguments = std::env::args().collect::<Vec<_>>();
    if arguments.get(1).map(String::as_str) == Some("generate-merged-kotlin") {
        if let Err(error) = merged_kotlin(&arguments[2..]) {
            eprintln!("nmp-nip46-uniffi-bindgen: {error}");
            std::process::exit(1);
        }
    } else {
        uniffi::uniffi_bindgen_main()
    }
}
