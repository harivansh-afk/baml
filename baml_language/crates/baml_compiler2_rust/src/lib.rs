//! Generate resumable Rust bodies for a checked, linked BAML program.
//!
//! Admission is per function. Unsupported bodies retain bytecode. Broken MIR
//! or missing linked identities fail the build instead of hiding behind fallback.

use std::{collections::HashMap, fmt::Write as _};

use baml_base::SourceRoot;
use baml_compiler2_hir::{
    item_data::{function_data, method_owner},
    loc::{DeclRef, FunctionLoc},
};
use baml_compiler2_hir_ty::extern_loc::FunctionRef;
use baml_compiler2_mir::{
    Constant, MirFunctionBody, MirFunctionKind, Operand, OptLevel, Place, Terminator,
    lower_function,
};
use baml_type::{Literal, RuntimeTy};
use bex_vm_types::{ConstValue, DeclPath, GlobalIndex, ObjectIndex, Program, compiled::CallTarget};

mod direct;
mod function;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeType {
    Int,
    Bool,
    IntArray,
}

impl NativeType {
    fn from_runtime<N: Clone>(ty: &RuntimeTy<N>) -> Option<Self> {
        match ty {
            RuntimeTy::Int | RuntimeTy::Literal(Literal::Int(_), _) => Some(Self::Int),
            RuntimeTy::Bool | RuntimeTy::Literal(Literal::Bool(_), _) => Some(Self::Bool),
            RuntimeTy::List(element) if matches!(element.as_ref(), RuntimeTy::Int) => {
                Some(Self::IntArray)
            }
            _ => None,
        }
    }
    fn rust(self) -> &'static str {
        match self {
            Self::Int => "Int63",
            Self::Bool => "bool",
            Self::IntArray => "Value",
        }
    }
    fn zero(self) -> &'static str {
        match self {
            Self::Int => "Int63::ZERO",
            Self::Bool => "false",
            Self::IntArray => "Value::NULL",
        }
    }
    fn read(self, value: &str) -> String {
        match self {
            Self::Int => format!("compiled::read_int({value})?"),
            Self::Bool => format!("compiled::read_bool({value})?"),
            Self::IntArray => format!("runtime.check_int_array({value})?"),
        }
    }
    fn value(self, value: &str) -> String {
        match self {
            Self::Int => format!("Value::int(({value}).get())"),
            Self::Bool => format!("Value::bool({value})"),
            Self::IntArray => value.into(),
        }
    }
}

#[derive(Debug)]
pub struct NativeModule {
    pub source: String,
    /// Linked object indices that have generated implementations.
    pub compiled: Vec<ObjectIndex>,
    pub fallback: Vec<Unsupported>,
    /// Direct-call eligibility for each admitted native function.
    pub direct_calls: Vec<DirectSupport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectSupport {
    pub function: String,
    pub eligible: bool,
    pub reason: String,
}

struct Admitted<'db> {
    loc: FunctionLoc<'db>,
    name: String,
    object: ObjectIndex,
    global: GlobalIndex,
    candidate: Candidate<'db>,
    calls: function::ResolvedCalls<'db>,
    prepared: function::PreparedFunction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub function: String,
    pub reason: String,
}

/// An invariant violation, not a request to interpret this function instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub function: String,
    pub reason: String,
}
impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.function, self.reason)
    }
}
impl std::error::Error for CompileError {}

#[derive(Debug)]
pub(crate) enum Rejection {
    Unsupported(String),
    Invalid(String),
}
impl Rejection {
    fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported(reason.into())
    }
    fn invalid(reason: impl Into<String>) -> Self {
        Self::Invalid(reason.into())
    }
}

struct Candidate<'db> {
    arity: usize,
    body: &'db MirFunctionBody<'db>,
    types: Vec<NativeType>,
    span: Option<baml_base::Span>,
}

/// Generate overrides for selected source functions in this exact linked image.
/// `package_roots` must come from the program driver's `LinkedProgram`; it must
/// not be reconstructed from package names. The database must be checked first.
/// The driver supplies the current source hash and canonical declaration
/// coordinates; this backend does not query or depend on the bytecode emitter.
/// Generated `install` verifies the entire image before installing any code.
pub fn emit_module<'db>(
    db: &'db dyn baml_compiler2_mir::Db,
    program: &Program,
    package_roots: &[SourceRoot],
    functions: &[FunctionLoc<'db>],
    source_content_hash: [u8; 32],
    function_address: impl Fn(FunctionRef<'db>) -> Option<(SourceRoot, DeclPath)>,
) -> Result<NativeModule, CompileError> {
    let error = |reason: String| CompileError {
        function: "<program>".into(),
        reason,
    };
    if program.packages.len() != package_roots.len() {
        return Err(error(
            "linked package identity map has the wrong length".into(),
        ));
    }
    program.validate().map_err(|e| error(e.to_string()))?;
    if program.source_content_hash != Some(source_content_hash) {
        return Err(error(
            "source database does not match the linked program".into(),
        ));
    }
    let roots: HashMap<_, _> = package_roots
        .iter()
        .enumerate()
        .map(|(i, r)| (*r, i))
        .collect();
    if roots.len() != package_roots.len() {
        return Err(error("duplicate linked package identity".into()));
    }
    let resolve = |reference: FunctionRef<'db>| -> Result<(GlobalIndex, ObjectIndex), Rejection> {
        let (root, path) = function_address(reference)
            .ok_or_else(|| Rejection::unsupported("callable has no declaration slot"))?;
        let package = *roots.get(&root).ok_or_else(|| {
            Rejection::invalid("callable's package is absent from the linked image")
        })?;
        let slot = program.packages[package]
            .global_slot(&path)
            .ok_or_else(|| {
                Rejection::invalid("callable declaration is absent from the linked image")
            })?;
        let Some(ConstValue::Object(object)) = program.globals.get(slot.raw()) else {
            return Err(Rejection::invalid(
                "callable declaration is not an object global",
            ));
        };
        if !matches!(
            program.objects.get(object.raw()),
            Some(bex_vm_types::Object::Function(_))
        ) {
            return Err(Rejection::invalid(
                "callable declaration is not a function object",
            ));
        }
        Ok((slot, *object))
    };
    let mut source = String::from(
        "#[allow(unused_imports)]\nuse bex_vm_types::{compiled::{self, CallTarget, SiteId, CompiledAction, CompiledCode, CompiledFrame, CompiledHeap, CompiledRuntime, CompiledSite, ResumeInput, SiteKind}, errors::{VmInternalError, VmPanic, VmRustFnError}, int::{self, Int63}, EarlyYieldCheck, GlobalIndex, ObjectIndex, HeapPtr, RootHaver, Value};\nuse std::collections::HashMap;\n",
    );
    let mut compiled = Vec::new();
    let mut fallback = Vec::new();
    let mut admitted = Vec::new();
    for &loc in functions {
        let name = function_data(db, loc).name.to_string();
        let result = (|| {
            let candidate = candidate(db, loc)?;
            let (global, object) = resolve(DeclRef::Source(loc))?;
            if compiled.contains(&object) {
                return Err(Rejection::invalid("duplicate source candidate"));
            }
            let mut calls = HashMap::new();
            let mut slots = Vec::new();
            for block in &candidate.body.blocks {
                if let Some(call @ Terminator::Call { .. }) = &block.terminator {
                    let callee = direct_callee(call).map_err(Rejection::unsupported)?;
                    if let std::collections::hash_map::Entry::Vacant(entry) = calls.entry(callee) {
                        let (slot, _) = resolve(callee)?;
                        entry.insert(CallTarget::from_raw(slots.len()));
                        slots.push(slot);
                    }
                }
            }
            let calls = function::ResolvedCalls { ids: calls, slots };
            // Establish normal admission before call-graph specialization. A
            // callee rejected by emission must never gain a direct entry point.
            let prepared = function::prepare(&candidate, &calls, loc.file(db).text(db))?;
            Ok((object, global, candidate, calls, prepared))
        })();
        match result {
            Ok((object, global, candidate, calls, prepared)) => {
                compiled.push(object);
                admitted.push(Admitted {
                    loc,
                    name,
                    object,
                    global,
                    candidate,
                    calls,
                    prepared,
                });
            }
            Err(Rejection::Unsupported(reason)) => fallback.push(Unsupported {
                function: name,
                reason,
            }),
            Err(Rejection::Invalid(reason)) => {
                return Err(CompileError {
                    function: name,
                    reason,
                });
            }
        }
    }
    let (targets, direct_calls) = direct::analyze(&admitted);
    for function in &admitted {
        let emit_error = |error: Rejection| CompileError {
            function: function.name.clone(),
            reason: match error {
                Rejection::Unsupported(reason) | Rejection::Invalid(reason) => reason,
            },
        };
        source.push_str(
            &function::emit(
                function.object,
                &function.candidate,
                &function.calls,
                &function.prepared,
                &targets,
            )
            .map_err(emit_error)?,
        );
        if let Some(target) = targets.get(&DeclRef::Source(function.loc)) {
            source.push_str(
                &function::emit_direct(
                    target,
                    &function.candidate,
                    &function.calls,
                    &function.prepared,
                    &targets,
                )
                .map_err(emit_error)?,
            );
        }
    }
    let fingerprint =
        bex_vm_types::compiled::program_fingerprint(program).map_err(|e| error(e.to_string()))?;
    source.push_str("\npub fn install(program: &mut bex_vm_types::Program) -> Result<(), VmInternalError> {\n    compiled::install(program, ");
    let _ = writeln!(source, "{fingerprint:?}, &[");
    for object in &compiled {
        let _ = writeln!(
            source,
            "        compiled::CompiledBinding {{ object: ObjectIndex::from_raw({object}), code: &CODE_{object} }},"
        );
    }
    source.push_str("    ])\n}\n");
    Ok(NativeModule {
        source,
        compiled,
        fallback,
        direct_calls,
    })
}

fn candidate<'db>(
    db: &'db dyn baml_compiler2_mir::Db,
    loc: FunctionLoc<'db>,
) -> Result<Candidate<'db>, Rejection> {
    let data = function_data(db, loc);
    if baml_compiler2_hir_ty::infer::trace_hooks::declaration_plan(db, loc).is_some() {
        return Err(Rejection::unsupported(
            "declared trace hook: selection prologue remains bytecode",
        ));
    }
    if !data.generic_params.is_empty() {
        return Err(Rejection::unsupported("generic function"));
    }
    if data.params.iter().any(|p| p.has_default) {
        return Err(Rejection::unsupported("default parameter"));
    }
    if method_owner(db, loc).is_some() {
        return Err(Rejection::unsupported("method"));
    }
    let mir = lower_function(db, loc, OptLevel::One)
        .as_ref()
        .map_err(|e| Rejection::invalid(e.to_string()))?;
    let MirFunctionKind::Bytecode(body) = &mir.kind else {
        return Err(Rejection::unsupported("builtin"));
    };
    if !mir.lambdas.is_empty() {
        return Err(Rejection::unsupported("closure"));
    }
    if body
        .blocks
        .iter()
        .any(|b| b.unwind.is_some() || b.landing.is_some() || b.handling.is_some() || b.shielded)
    {
        return Err(Rejection::unsupported("catch, defer or error handling"));
    }
    let mut types = Vec::new();
    for (i, local) in body.locals.iter().enumerate() {
        if local.is_captured {
            return Err(Rejection::unsupported("captured local"));
        }
        if (1..=mir.arity).contains(&i) && matches!(local.ty, RuntimeTy::Literal(..)) {
            return Err(Rejection::unsupported("literal parameter type"));
        }
        types.push(
            NativeType::from_runtime(&local.ty)
                .ok_or_else(|| Rejection::unsupported(format!("type {:?}", local.ty)))?,
        );
    }
    if types.len() <= mir.arity {
        return Err(Rejection::invalid("malformed MIR signature"));
    }
    Ok(Candidate {
        arity: mir.arity,
        body,
        types,
        span: mir.span,
    })
}

pub(crate) fn direct_callee<'db>(call: &Terminator<'db>) -> Result<FunctionRef<'db>, &'static str> {
    let Terminator::Call {
        has_trace,
        argument_layout,
        callee,
        args,
        ntypeargs,
        destination,
        unwind,
        ..
    } = call
    else {
        return Err("not a call");
    };
    if *has_trace {
        return Err("call with explicit trace attachment");
    }
    if *ntypeargs != 0 {
        return Err("call with type arguments");
    }
    if unwind.is_some() {
        return Err("call inside a catch");
    }
    if argument_layout
        .as_ref()
        .is_some_and(|layout| layout.0.len() != args.len() || layout.0.iter().any(Option::is_some))
    {
        return Err("call with named or omitted arguments");
    }
    if !matches!(destination, Place::Local(_)) {
        return Err("call result stored outside a local");
    }
    match callee {
        Operand::Constant(Constant::Function(loc)) => Ok(*loc),
        _ => Err("indirect call"),
    }
}
