//! Real-relay application falsifier for NMP's NIP-29 facade (#1201, #1140).
//!
//! Croissant and the runner only stage the outside world. Every query,
//! subscription, cache read, provenance merge, route, signature and receipt
//! inspected here crosses the supported `nmp` facade.

mod args;
mod observe;
mod probe;

fn main() {
    let args = args::Args::parse_or_exit();
    if let Err(error) = probe::run(args) {
        eprintln!("FAIL {error}");
        std::process::exit(1);
    }
}
