//! FFI bindings to the vendored MIT libghostty-vt C API.
//!
//! Phase 2a exposes the raw bindgen-generated symbols only (build plumbing).
//! A safe wrapper (`GhosttyScreen`) lands in Phase 2b.

#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all
)]
pub mod sys {
    include!(concat!(env!("OUT_DIR"), "/ghostty_bindings.rs"));
}
