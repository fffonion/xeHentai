//! Normal Cargo build metadata for the native update contract.
//!
//! Three values are compiled in, never read at runtime: the target triple the
//! binary was built for (used to accept or reject a native package) and an
//! optional release/update identifier (`XEH_BUILD_ID`) plus its public label.
//! Unset means an unpublished `<Cargo version>-dev` build; a release reports
//! the supplied tag directly. No repository/file hash or runtime override.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=XEH_BUILD_ID");
    let target = env::var("TARGET").expect("cargo build target");
    println!("cargo:rustc-env=XEH_TARGET={target}");
    let build_id = env::var("XEH_BUILD_ID").unwrap_or_else(|_| "dev".to_string());
    println!("cargo:rustc-env=XEH_BUILD_ID={build_id}");
    let display = if build_id == "dev" {
        format!(
            "{}-dev",
            env::var("CARGO_PKG_VERSION").expect("Cargo package version")
        )
    } else {
        build_id
    };
    println!("cargo:rustc-env=XEH_DISPLAY_VERSION={display}");
    // The task loop, the runtime and the guest VM all run on the process' main
    // thread. Unix gives that thread 8 MiB, Windows only 1 MiB, and a debug
    // build of the guest frames a download deep enough to overflow it
    // (0xC00000FD). Ask the linker for the Unix-sized stack so both platforms
    // start from the same budget.
    if target.ends_with("windows-msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    } else if target.ends_with("windows-gnu") {
        println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608");
    }
}
