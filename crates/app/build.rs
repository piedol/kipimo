//! `cfg(dev)`: the development-only URL commands (`kipimo://demo/…`, `selftest`, `summary`,
//! `close-windows`, …) and the demo hooks behind them. On in debug builds and with the
//! `dev-urls` feature (a release-optimised build for measuring); off in release builds.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(dev)");
    println!("cargo::rerun-if-changed=build.rs");
    let debug = std::env::var("PROFILE").is_ok_and(|p| p == "debug")
        || std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
    if debug || std::env::var_os("CARGO_FEATURE_DEV_URLS").is_some() {
        println!("cargo::rustc-cfg=dev");
    }
}
