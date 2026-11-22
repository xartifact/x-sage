# Pinned protocol build tools

The single service.proto produces Rust binary types and standard protobuf JSON via prost0.14.6/pbjson0.9.0, and TypeScript interfaces via ts-proto2.6.1. Runtime handlers are separate. No handwritten persistence codec or parallel DTO is introduced.

Run scripts/setup-sage-db-codegen.sh with an absolute operator-owned tooling directory, then source its env.sh. It downloads protoc36.0 from the official protocolbuffers/protobuf release and verifies the pinned release-asset SHA256 before extraction. It installs the isolated Node plugin with pnpm using the committed tools/protocol-codegen package and frozen lockfile, ignoring lifecycle scripts. It changes no default Rust/Node tools or application/KB data. --compiler-only provisions only the Rust build dependency, never a claim that TypeScript generation passed. Supported compiler archive host names do not qualify storage durability on those hosts.

Cargo build.rs and the TypeScript generator both demand exactly libprotoc36.0. The generator checks installed plugin identity2.6.1 and byte-compares regenerated TS in --check mode. SAGE_DB_PROTOC and SAGE_DB_TS_PROTO_PLUGIN explicitly name tools; neither is a durability capability override. Without an explicit plugin the generator can still find the documented isolated protocol-tooling directory. CI provisions the pinned tools before mandatory generation, type/test and Rust checks. A missing compiler/plugin or output drift fails the gate, never skips.

Release asset identities are from https://github.com/protocolbuffers/protobuf/releases/tag/v36.0; tool setup uses public content only. Source identity does not include arbitrary contents of the tooling directory: capture the compiler/plugin versions and release checksums alongside exact-source gate evidence. The pinned Node lock binds transitive plugin dependencies.

Raw network input size/decoder allocation bounds, service execution, authentication configuration and deployment are independent requirements. This tooling is not P4 acceptance or real-KB readiness.
