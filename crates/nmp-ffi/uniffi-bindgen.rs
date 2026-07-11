//! The bindgen entry point (M4 plan §3 step 3): `cargo run -p nmp-ffi --bin
//! uniffi-bindgen generate --library <path-to-libnmp_ffi.a> --language swift
//! --out-dir gen/`. Library mode reads the exported UniFFI metadata straight
//! out of the already-compiled staticlib -- no `.udl` file, no separate
//! scaffolding-generation step.

fn main() {
    uniffi::uniffi_bindgen_main()
}
