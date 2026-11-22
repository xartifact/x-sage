//! Generate the Rust command-protocol types from the single proto authority.
//!
//! Two generators run over `proto/sage/v1/service.proto`:
//!
//! * `prost-build` emits the binary structs;
//! * `pbjson-build` emits the **standard protobuf JSON** `Serialize`/`Deserialize`
//!   implementations for those same structs.
//!
//! Both are derived, never hand-written: a second DTO model beside the generated one would
//! be a second authority for the wire format.
//!
//! `protoc` is located as follows, in order:
//!
//! 1. `SAGE_DB_PROTOC` if set — an explicit override for a vendored or pinned compiler;
//! 2. `/opt/homebrew/bin/protoc` if it exists, because that is the installed compiler on
//!    this workspace's development host and pinning the path keeps the build reproducible
//!    across shells whose `PATH` may differ;
//! 3. `protoc` from `PATH`.
//!
//! The override is applied through `prost_build::Config::protoc_executable`, which is a
//! safe configuration method. This crate forbids `unsafe`, so `std::env::set_var` — unsafe
//! in edition 2024 — is not used and must not be introduced.

use std::path::{Path, PathBuf};

#[path = "build/ingress_codegen.rs"]
mod ingress_codegen;

fn main() {
    let proto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let proto = proto_dir.join("sage/v1/service.proto");

    println!("cargo:rerun-if-changed={}", proto.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SAGE_DB_PROTOC");

    let protoc = resolve_protoc();
    let version = std::process::Command::new(&protoc)
        .arg("--version")
        .output()
        .expect("the pinned protoc compiler must be executable");
    assert!(version.status.success(), "protoc --version failed");
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "libprotoc 36.0",
        "this protocol build pins protoc 36.0; use SAGE_DB_PROTOC for an explicit verified compiler"
    );

    // The include path is the proto ROOT, so the file's own package-qualified name is what
    // generated code refers to.
    let mut config = prost_build::Config::new();
    config.protoc_executable(&protoc);
    config.file_descriptor_set_path(
        Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"))
            .join("sage_descriptor.bin"),
    );
    config
        .compile_protos(&[&proto], &[&proto_dir])
        .unwrap_or_else(|error| panic!("prost-build failed for {}: {error}", proto.display()));

    // Admitted seeds: a THIRD generator over the SAME descriptor set, so the admitted decoder
    // and the standard one cannot disagree about the message shapes.
    let descriptor = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"))
        .join("sage_descriptor.bin");
    ingress_codegen::generate(
        &descriptor,
        &PathBuf::from(&std::env::var("OUT_DIR").unwrap()),
    );

    // Standard protobuf JSON for the types prost just emitted, from the same descriptor set.
    pbjson_build::Builder::new()
        .register_descriptors(
            &std::fs::read(
                Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"))
                    .join("sage_descriptor.bin"),
            )
            .expect("the descriptor set prost-build just wrote"),
        )
        .expect("the descriptor set is readable")
        .build(&[".sage.v1"])
        .expect("pbjson-build failed for sage.v1");
}

/// The `protoc` this build will use, resolved without mutating the process environment.
fn resolve_protoc() -> PathBuf {
    if let Some(explicit) = std::env::var_os("SAGE_DB_PROTOC") {
        let path = PathBuf::from(explicit);
        assert!(
            path.is_file(),
            "SAGE_DB_PROTOC points at {}, which is not a file",
            path.display()
        );
        return path;
    }
    let homebrew = PathBuf::from("/opt/homebrew/bin/protoc");
    if homebrew.is_file() {
        return homebrew;
    }
    // Fall back to PATH lookup by asking prost-build for its own default.
    PathBuf::from("protoc")
}
