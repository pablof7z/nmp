use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(
        env::var_os("NOSTRDB_DIR").expect("NOSTRDB_DIR must point at the pinned nostrdb checkout"),
    );
    for required in [
        "libnostrdb.a",
        "deps/lmdb/liblmdb.a",
        "deps/libsodium/src/libsodium/.libs/libsodium.a",
        "deps/secp256k1/.libs/libsecp256k1.a",
    ] {
        let path = root.join(required);
        assert!(
            path.is_file(),
            "missing {}; build the pinned nostrdb checkout first",
            path.display()
        );
    }

    cc::Build::new()
        .file("src/nostrdb_bridge.c")
        .include(root.join("src"))
        .include(root.join("deps/lmdb"))
        .include(root.join("ccan"))
        .include(root.join("deps/flatcc/include"))
        .warnings(true)
        .opt_level(2)
        .compile("nostrdb_bench_bridge");

    println!("cargo:rustc-link-search=native={}", root.display());
    println!(
        "cargo:rustc-link-search=native={}",
        root.join("deps/lmdb").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        root.join("deps/libsodium/src/libsodium/.libs").display()
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy(
        root.join("deps/secp256k1/.libs/libsecp256k1.a"),
        out_dir.join("libndb_secp256k1.a"),
    )
    .expect("copy pinned secp256k1 under an unambiguous link name");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=nostrdb");
    println!("cargo:rustc-link-lib=static=lmdb");
    println!("cargo:rustc-link-lib=static=sodium");
    println!("cargo:rustc-link-lib=static=ndb_secp256k1");
    println!("cargo:rustc-link-lib=dylib=pthread");
    println!("cargo:rustc-link-lib=dylib=m");
    println!("cargo:rustc-link-lib=dylib=dl");
    println!("cargo:rerun-if-changed=src/nostrdb_bridge.c");
    println!("cargo:rerun-if-env-changed=NOSTRDB_DIR");
    println!("cargo:rustc-env=NOSTRDB_DIR_PINNED={}", root.display());
}
