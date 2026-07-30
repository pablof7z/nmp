//! Print one named UniFFI component interface embedded in a native library.
//!
//! This standalone governance tool is deliberately outside the NMP workspace
//! and product crate. Its source, manifest, and lockfile are trusted from the
//! PR base. The output comes from proc-macro metadata in library mode, not UDL.

use std::{env, io};

use uniffi_bindgen::{library_mode, EmptyCrateConfigSupplier};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 4 {
        return Err(io::Error::other(
            "usage: nmp-component-interface-snapshot <library> <component-key> <uniffi-namespace>",
        )
        .into());
    }
    let library = &args[1];
    let component_key = &args[2];
    let namespace = &args[3];

    let components =
        library_mode::find_components(library.as_str().into(), &EmptyCrateConfigSupplier)?;
    let matches = components
        .iter()
        .filter(|component| component.ci.namespace() == namespace)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(io::Error::other(format!(
            "expected exactly one UniFFI component namespace {namespace:?} for {component_key}; found {}",
            matches.len()
        ))
        .into());
    }
    let component = matches[0];
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
