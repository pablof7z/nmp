//! Print the UniFFI component interface embedded in an NMP native library.
//!
//! This standalone governance tool is deliberately outside the NMP workspace
//! and product crate. Its source, manifest, and lockfile are trusted from the
//! PR base. The output comes from proc-macro metadata in library mode, not UDL.

use std::{env, io};

use uniffi_bindgen::{library_mode, EmptyCrateConfigSupplier};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let library = env::args()
        .nth(1)
        .ok_or_else(|| io::Error::other("usage: nmp-component-interface-snapshot <library>"))?;

    let mut components =
        library_mode::find_components(library.as_str().into(), &EmptyCrateConfigSupplier)?;
    let component = components
        .iter_mut()
        .find(|component| component.ci.crate_name() == "nmp_ffi")
        .ok_or_else(|| io::Error::other("nmp_ffi component metadata not found"))?;
    let ci = &component.ci;

    println!("# NMP UniFFI component interface");
    println!("# source: proc-macro metadata extracted in library mode (not UDL)");
    println!("# uniffi: 0.29.5");
    println!("namespace {:?}", ci.namespace());

    for definition in ci.enum_definitions() {
        println!("\nenum {:#?}", definition);
    }
    for definition in ci.record_definitions() {
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
