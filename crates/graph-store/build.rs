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

    // Downloads live in a location that outlasts `cargo clean` and CI
    // target-dir eviction (rust-cache drops workspace crates' OUT_DIRs but
    // preserves $CARGO_HOME) — otherwise every CI run would re-fetch and
    // the CDN flake this vendoring exists to kill would move to build time.
    let cache_dir = std::env::var("CARGO_HOME")
        .map(|home| {
            Path::new(&home)
                .join("graph-lbug-extensions")
                .join(format!("v{EXTENSION_VERSION}-{platform}"))
        })
        .unwrap_or_else(|_| PathBuf::from(std::env::var("OUT_DIR").unwrap()));
    std::fs::create_dir_all(&cache_dir)
        .unwrap_or_else(|e| panic!("creating {}: {e}", cache_dir.display()));

    for ext in EXTENSIONS {
        let file = format!("lib{ext}.lbug_extension");
        let dest = cache_dir.join(&file);
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
    let mut last_err = String::new();
    for attempt in 1..=3u32 {
        match try_download(url, dest) {
            Ok(()) => return,
            Err(e) => {
                last_err = e;
                eprintln!("attempt {attempt} downloading {url}: {last_err}");
                std::thread::sleep(std::time::Duration::from_secs(2 * u64::from(attempt)));
            }
        }
    }
    panic!(
        "downloading {url} failed after 3 attempts: {last_err}\nfor offline builds, set \
         GRAPH_LBUG_EXT_DIR to a directory of pre-fetched extension files"
    );
}

fn try_download(url: &str, dest: &Path) -> Result<(), String> {
    let response = ureq::get(url).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)
        .map_err(|e| e.to_string())?;
    // Write-then-rename with a per-process tmp name: an aborted or
    // concurrent build must never leave a truncated file at the final path
    // for a later `dest.exists()` check to trust.
    let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
}
