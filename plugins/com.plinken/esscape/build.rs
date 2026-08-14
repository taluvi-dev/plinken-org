// `wclap-host-js` scans the plugin's exports for a WebAssembly.Table to use
// as the function-table source, then grows it at runtime to install host
// trampolines. rust-lld's wasm linker defaults to a fixed-size table
// (max == initial), so we ask for it to be both exported *and* growable.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-cdylib-link-arg=--export-table");
        println!("cargo:rustc-cdylib-link-arg=--growable-table");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
