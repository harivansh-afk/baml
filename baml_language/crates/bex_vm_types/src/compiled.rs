//! Resumable compiled BAML bodies. The VM owns invocation, calls, unwinding
//! and scheduling; generated code owns only its locals and continuation.
//!
//! Descriptors are installed before loading an exact program image. Code
//! pointers are process-local and are never serialized into BAML artifacts.

use std::{collections::HashSet, sync::Arc};

use sha2::{Digest, Sha256};

use crate::{
    EarlyYieldCheck, FunctionKind, Object, Program, RootHaver, Value,
    errors::{VmInternalError, VmRustFnError},
};

/// The input to one bounded execution interval. A call result is delivered
/// exactly once; GC parking resumes with `Continue`.
#[derive(Debug)]
pub enum ResumeInput {
    Continue,
    Returned(Value),
}

/// These conversions guard the generated-code boundary. A mismatch is a
/// compiler/embedding fault: BAML call checking already established the type.
pub fn read_int(value: Value) -> Result<baml_type::Int63, VmInternalError> {
    value
        .as_int()
        .and_then(baml_type::Int63::new)
        .ok_or_else(|| VmInternalError::InvalidCompiledCode {
            message: "expected int at compiled call boundary".into(),
        })
}

pub fn read_bool(value: Value) -> Result<bool, VmInternalError> {
    value
        .as_bool()
        .ok_or_else(|| VmInternalError::InvalidCompiledCode {
            message: "expected bool at compiled call boundary".into(),
        })
}

/// Control returns to the VM before executing a callee or parking a task.
#[derive(Debug)]
pub enum CompiledAction {
    Return(Value),
    Call { target: usize, args: Vec<Value> },
    Yield,
}

/// Heap operations available during one compiled execution interval. The VM
/// holds its existing heap permit throughout the call. Operations never park
/// and return values rather than borrowed heap storage, so neither an array
/// lock nor a reference into the heap can survive a yield.
///
/// Arrays keep their BEX identity. Checking or passing an array does not copy
/// its elements; only `alloc_int_array` creates a new array.
pub trait CompiledHeap {
    fn check_int_array(&self, value: Value) -> Result<Value, VmInternalError>;
    fn alloc_int_array(&mut self, values: Vec<baml_type::Int63>) -> Value;
    fn int_array_len(&self, array: Value) -> Result<baml_type::Int63, VmRustFnError>;
    fn int_array_get(
        &self,
        array: Value,
        index: baml_type::Int63,
    ) -> Result<baml_type::Int63, VmRustFnError>;
    fn int_array_set(
        &mut self,
        array: Value,
        index: baml_type::Int63,
        value: baml_type::Int63,
    ) -> Result<(), VmRustFnError>;
}

/// Services available while generated code holds the VM's heap permit.
/// Direct calls keep their locals on the Rust stack; these hooks retain only
/// logical BAML invocation metadata. They never park or run another body.
pub trait CompiledRuntime: CompiledHeap {
    fn enter_direct(
        &mut self,
        caller_site: usize,
        global: usize,
        code: &'static CompiledCode,
        args: &[Value],
    ) -> Result<(), VmRustFnError>;

    fn return_direct(&mut self, result: Value) -> Result<(), VmRustFnError>;

    /// Leave failed activations for the existing BAML unwinder. A parent must
    /// not overwrite a deeper callee's faulting site while propagating an error.
    fn fail_direct(&mut self, code: &'static CompiledCode, site: usize);
}

/// State retained while a compiled BAML invocation is suspended. Every heap
/// reference in that state must participate in `RootHaver`, including state
/// retained while a bytecode callee executes.
pub trait CompiledFrame: RootHaver {
    fn resume(
        &mut self,
        input: ResumeInput,
        poll: &mut EarlyYieldCheck,
        runtime: &mut dyn CompiledRuntime,
    ) -> Result<CompiledAction, VmRustFnError>;

    /// Exact source site of the current operation, distinct from resume state.
    fn site(&self) -> usize;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteKind {
    Operation,
    Call,
}

#[derive(Clone, Copy, Debug)]
pub struct CompiledSite {
    pub file_id: u32,
    pub start: u32,
    pub end: u32,
    pub line: u32,
    pub kind: SiteKind,
}

#[derive(Debug)]
pub struct CompiledCode {
    pub create: FrameFactory,
    pub sites: &'static [CompiledSite],
    /// Absolute global slots in the exact linked image, indexed by `Call.target`.
    pub calls: &'static [usize],
}

pub type FrameFactory =
    fn(&[Value], &dyn CompiledHeap) -> Result<Box<dyn CompiledFrame>, VmInternalError>;

impl CompiledCode {
    pub fn source_map(&self) -> btel_types::SourceMap {
        btel_types::SourceMap {
            coordinate: btel_types::SourceCoordinate::CompiledSite,
            extent: u32::try_from(self.sites.len()).expect("validated compiled site count"),
            entries: self
                .sites
                .iter()
                .enumerate()
                .map(|(pc, site)| btel_types::SourceMapEntry {
                    pc: u32::try_from(pc).expect("validated compiled site count"),
                    file_id: site.file_id,
                    start: site.start,
                    end: site.end,
                    line: site.line,
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
pub struct CompiledBinding {
    pub object: usize,
    pub code: &'static CompiledCode,
}

/// The proof retained with an installed implementation. Cloning an unchanged
/// image preserves it; loading a modified image must validate it again.
#[derive(Debug)]
pub struct InstalledCode {
    code: &'static CompiledCode,
    fingerprint: [u8; 32],
    object: usize,
}

impl std::ops::Deref for InstalledCode {
    type Target = CompiledCode;
    fn deref(&self) -> &Self::Target {
        self.code
    }
}

/// Check process-local bindings before loader transformations. This also
/// catches descriptors copied to another object in an otherwise identical image.
pub fn validate_bindings(program: &Program) -> Result<(), VmInternalError> {
    if !program
        .objects
        .iter()
        .any(|object| matches!(object, Object::Function(f) if f.compiled.is_some()))
    {
        return Ok(());
    }
    let fingerprint =
        program_fingerprint(program).map_err(|e| VmInternalError::InvalidCompiledCode {
            message: e.to_string(),
        })?;
    for (index, object) in program.objects.iter().enumerate() {
        if let Object::Function(function) = object
            && let Some(code) = &function.compiled
            && (code.fingerprint != fingerprint
                || code.object != index
                || !function.runtime_package.is_null())
        {
            return Err(VmInternalError::InvalidCompiledCode {
                message: "compiled binding was moved or its image changed".into(),
            });
        }
    }
    Ok(())
}

/// Fingerprint the serialized program, excluding all process-local execution
/// and telemetry state through the artifact types' existing `borsh(skip)`.
pub fn program_fingerprint(program: &Program) -> Result<[u8; 32], std::io::Error> {
    Ok(Sha256::digest(borsh::to_vec(program)?).into())
}

/// Attach generated code atomically to its exact static image. A stale build,
/// duplicate binding, or invalid target is an error, never a partial install.
pub fn install(
    program: &mut Program,
    fingerprint: [u8; 32],
    bindings: &[CompiledBinding],
) -> Result<(), VmInternalError> {
    let invalid = |message: &str| VmInternalError::InvalidCompiledCode {
        message: message.into(),
    };
    program.validate()?;
    if program_fingerprint(program).map_err(|e| invalid(&e.to_string()))? != fingerprint {
        return Err(invalid(
            "compiled code does not belong to this program image",
        ));
    }
    let mut seen = HashSet::new();
    for binding in bindings {
        if !seen.insert(binding.object) {
            return Err(invalid("duplicate compiled binding"));
        }
        let Some(Object::Function(function)) = program.objects.get(binding.object) else {
            return Err(invalid("compiled target is not a function"));
        };
        if !matches!(function.kind, FunctionKind::Bytecode)
            || function.compiled.is_some()
            || !function.runtime_package.is_null()
            || !function.generic_param_bounds.is_empty()
            || !function.display_type_params.is_empty()
            || function.param_has_default.iter().any(|v| *v)
            || !function.bytecode.exception_table.is_empty()
            || !function.bytecode.shield_table.is_empty()
        {
            return Err(invalid(
                "compiled target has an unsupported invocation contract",
            ));
        }
        if binding.code.sites.is_empty()
            || binding.code.sites.len() >= u32::MAX as usize
            || binding
                .code
                .sites
                .iter()
                .any(|s| s.start > s.end || s.line == 0)
        {
            return Err(invalid("invalid compiled source sites"));
        }
        for &slot in binding.code.calls {
            let Some(crate::ConstValue::Object(index)) = program.globals.get(slot) else {
                return Err(invalid("compiled call target is not an object global"));
            };
            if !matches!(program.objects.get(index.raw()), Some(Object::Function(_))) {
                return Err(invalid("compiled call target is not a function"));
            }
        }
    }
    for binding in bindings {
        let Object::Function(function) =
            &mut program.objects[crate::ObjectIndex::from_raw(binding.object)]
        else {
            unreachable!("validated before mutation")
        };
        function.compiled = Some(Arc::new(InstalledCode {
            code: binding.code,
            fingerprint,
            object: binding.object,
        }));
    }
    Ok(())
}
