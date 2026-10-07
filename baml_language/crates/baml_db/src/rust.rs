//! Rust output through the compiler's public driver boundary.
//!
//! The bytecode emitter currently owns canonical declaration-coordinate and
//! source-identity queries. Resolve those here, alongside the linked layout,
//! without making the Rust emitter depend on the bytecode emitter. The feature
//! is opt-in so a runtime compiler that only emits bytecode stays independent
//! of Rust generation.

use baml_compiler2_hir::loc::FunctionLoc;
pub use baml_compiler2_rust::{CompileError, DirectSupport, NativeModule, Unsupported};

use crate::{ProjectDatabase, program::LinkedProgram};

/// Emit native overrides for a checked database and its exact linked image.
/// The backend validates the live source identity, package mapping and every
/// used callable address before emitting bindings.
pub fn emit_module<'db>(
    db: &'db ProjectDatabase,
    linked: &LinkedProgram,
    functions: &[FunctionLoc<'db>],
) -> Result<NativeModule, CompileError> {
    let root = linked
        .package_roots
        .get(linked.program.root as usize)
        .copied()
        .ok_or_else(|| CompileError {
            function: "<program>".into(),
            reason: "linked program has no compiler root identity".into(),
        })?;
    baml_compiler2_rust::emit_module(
        db,
        &linked.program,
        &linked.package_roots,
        functions,
        baml_compiler2_emit::project_source_content_hash(db, root),
        |reference| baml_compiler2_emit::function_address(db, reference),
    )
}
