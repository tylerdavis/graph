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
use std::path::{Path, PathBuf};

/// Must match what the built lbug engine expects. The vendored CMakeLists
/// says 0.18.0, but the runtime library requests v0.18.1 extension URLs —
/// trust the runtime.
const EXTENSION_VERSION: &str = "0.18.1";
const EXTENSIONS: &[&str] = &["fts", "vector"];

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

    vendor_extensions();
}

/// Fetch the lbug extension binaries (FTS, VECTOR) for the target platform
/// so `src/extensions.rs` can embed them — the runtime then loads them by
/// path with zero network access. Downloads land in OUT_DIR and are reused
/// across builds; set `GRAPH_LBUG_EXT_DIR` to a directory of pre-fetched
/// `lib<ext>.lbug_extension` files for offline/hermetic builds.
fn vendor_extensions() {
    println!("cargo:rerun-if-env-changed=GRAPH_LBUG_EXT_DIR");
    println!("cargo:rustc-env=GRAPH_LBUG_EXT_VERSION={EXTENSION_VERSION}");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let platform = match (
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
    ) {
        (Ok("macos"), Ok("aarch64")) => "osx_arm64",
        (Ok("macos"), Ok("x86_64")) => "osx_amd64",
        (Ok("linux"), Ok("x86_64")) => "linux_amd64",
        (Ok("linux"), Ok("aarch64")) => "linux_arm64",
        (os, arch) => panic!(
            "no known lbug extension platform for {os:?}/{arch:?} — set GRAPH_LBUG_EXT_DIR \
             to a directory containing lib<ext>.lbug_extension files"
        ),
    };

    for ext in EXTENSIONS {
        let file = format!("lib{ext}.lbug_extension");
        let dest = out_dir.join(&file);
        if let Ok(dir) = std::env::var("GRAPH_LBUG_EXT_DIR") {
            let src = Path::new(&dir).join(&file);
            std::fs::copy(&src, &dest).unwrap_or_else(|e| {
                panic!(
                    "GRAPH_LBUG_EXT_DIR is set but copying {} failed: {e}",
                    src.display()
                )
            });
        } else if !dest.exists() {
            let url = format!(
                "https://extension.ladybugdb.com/v{EXTENSION_VERSION}/{platform}/{ext}/{file}"
            );
            download(&url, &dest);
        }
        println!(
            "cargo:rustc-env=GRAPH_LBUG_EXT_{}={}",
            ext.to_uppercase(),
            dest.display()
        );
    }
}

fn download(url: &str, dest: &Path) {
    let response = ureq::get(url).call().unwrap_or_else(|e| {
        panic!(
            "downloading {url}: {e}\nfor offline builds, set GRAPH_LBUG_EXT_DIR to a \
             directory of pre-fetched extension files"
        )
    });
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)
        .unwrap_or_else(|e| panic!("reading {url}: {e}"));
    // Write-then-rename so an aborted build can't leave a truncated file
    // that a later `dest.exists()` check would trust.
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, &bytes).unwrap_or_else(|e| panic!("writing {}: {e}", tmp.display()));
    std::fs::rename(&tmp, dest).unwrap_or_else(|e| panic!("renaming into {}: {e}", dest.display()));
}
