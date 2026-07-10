/// The graph binary dlopens lbug extensions (FTS, VECTOR), which bind
/// against symbols the host must export. See crates/graph-store/build.rs
/// for the full story.
fn main() {
    let export = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        "-Wl,-export_dynamic"
    } else {
        "-Wl,--export-dynamic"
    };
    println!("cargo:rustc-link-arg-bins={export}");
}
