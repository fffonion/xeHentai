//! Normal Cargo build metadata for the native update contract.
//!
//! Two values are compiled in, never read at runtime: the target triple the
//! binary was built for (used to accept or reject a native package) and an
//! optional release/update identifier (`XEH_BUILD_ID`). Unset means `dev`, so
//! the ordinary unpublished build keeps its normal version output; a release
//! build passes its identifier once and the identifier appears in the existing
//! version label. No repository/file hash is used and there is no runtime
//! override.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=XEH_BUILD_ID");
    let target = env::var("TARGET").expect("cargo build target");
    println!("cargo:rustc-env=XEH_TARGET={target}");
    let build_id = env::var("XEH_BUILD_ID").unwrap_or_else(|_| "dev".to_string());
    println!("cargo:rustc-env=XEH_BUILD_ID={build_id}");
}
