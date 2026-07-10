/// lbug 0.18's prebuilt library needs OpenSSL but emits no link directives
/// (fixed upstream post-0.18.0), and its dlopen'd extensions (FTS, VECTOR)
/// require the host binary to export its symbols. Emitting these as build
/// directives — rather than env-wide RUSTFLAGS — keeps them scoped to real
/// artifacts (RUSTFLAGS poisons build scripts; segfaults observed on
/// x86_64 Linux CI). Details: crates/graph-store/SPIKE.md.
///
/// `rustc-link-lib` propagates to downstream binaries via rlib metadata,
/// but does NOT reach this package's own test binaries — those get the
/// libs via raw `rustc-link-arg-tests` instead.
fn main() {
    if let Ok(out) = std::process::Command::new("pkg-config")
        .args(["--variable=libdir", "openssl"])
        .output()
    {
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !dir.is_empty() {
            println!("cargo:rustc-link-search=native={dir}");
        }
    }
    println!("cargo:rustc-link-lib=dylib=ssl");
    println!("cargo:rustc-link-lib=dylib=crypto");

    let export = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        "-Wl,-export_dynamic"
    } else {
        "-Wl,--export-dynamic"
    };
    println!("cargo:rustc-link-arg-tests={export}");
    println!("cargo:rustc-link-arg-tests=-lssl");
    println!("cargo:rustc-link-arg-tests=-lcrypto");
}
