//! Execution of compiled bodies through the ordinary BAML invocation lifecycle.

use bex_vm_types::{
    GlobalIndex, Object, RealizedTy, StackIndex, Value,
    compiled::{
        CompiledAction, CompiledCode, CompiledFrame, CompiledHeap, CompiledRuntime, ResumeInput,
    },
    errors::{VmInternalError, VmPanic, VmRustFnError},
    int::Int63,
};

use super::{
    BamlFrame, BexVm, Frame, Function, FunctionType, InvocationOutcome, MAX_FRAMES, VmError,
    VmExecState,
};
use crate::indexable::EvalStackTrait;

pub(super) enum FrameExecution {
    /// Arguments are rooted on the eval stack until the body is initialized.
    Start,
    Bytecode(usize),
    Compiled(Box<dyn CompiledFrame>),
    /// Generated code is on the Rust stack under the active heap permit.
    Active,
}

impl std::fmt::Debug for FrameExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start => f.write_str("Start"),
            Self::Active => f.write_str("Active"),
            Self::Bytecode(pc) => f.debug_tuple("Bytecode").field(pc).finish(),
            Self::Compiled(frame) => f
                .debug_struct("Compiled")
                .field("site", &frame.site())
                .finish(),
        }
    }
}

impl BamlFrame {
    pub fn bytecode_pc(&self) -> Option<usize> {
        match self.execution {
            FrameExecution::Start => Some(0),
            FrameExecution::Bytecode(pc) => Some(pc),
            FrameExecution::Compiled(_) | FrameExecution::Active => None,
        }
    }

    pub(super) fn set_bytecode_pc(&mut self, pc: usize) {
        assert!(
            !matches!(
                self.execution,
                FrameExecution::Compiled(_) | FrameExecution::Active
            ),
            "bytecode dispatch on a compiled frame"
        );
        self.execution = FrameExecution::Bytecode(pc);
    }
}

fn invalid(message: &str) -> VmError {
    VmInternalError::InvalidCompiledCode {
        message: message.into(),
    }
    .into()
}

// The VM implements these services only while run_compiled holds its permit.
// Helpers never release the permit or retain borrowed heap storage.
impl CompiledRuntime for BexVm {
    fn enter_direct(
        &mut self,
        caller_site: usize,
        global: usize,
        code: &'static CompiledCode,
        args: &[Value],
    ) -> Result<(), VmRustFnError> {
        let invalid = |message: &str| VmInternalError::InvalidCompiledCode {
            message: message.into(),
        };
        let caller_idx = self
            .frames
            .len()
            .checked_sub(1)
            .ok_or_else(|| invalid("direct call has no caller"))?;
        let Frame::Baml(caller) = &mut self.frames[caller_idx] else {
            return Err(invalid("direct call requires a BAML caller").into());
        };
        if !matches!(caller.execution, FrameExecution::Active) {
            return Err(invalid("direct call requires an active generated body").into());
        }
        caller.faulting_pc = caller_site;
        self.cur_pc = caller_site;
        if self.frames.len() >= MAX_FRAMES {
            return Err(VmPanic::StackOverflow.into());
        }
        let value = self
            .globals
            .as_slice(self.proof())
            .get(global)
            .copied()
            .ok_or_else(|| invalid("direct callee global is absent"))?;
        let pointer = value
            .as_object_ptr()
            .ok_or_else(|| invalid("direct callee is not a function"))?;
        // SAFETY: globals root the callee and the active permit prevents moving
        // collection until the outer generated body returns to the engine.
        let Object::Function(callee) = (unsafe { pointer.get() }) else {
            return Err(invalid("direct callee is not a function").into());
        };
        if !callee.runtime_package.is_null()
            || !callee.compiled.as_ref().is_some_and(|installed| {
                std::ptr::eq(
                    std::ptr::from_ref::<CompiledCode>(installed),
                    std::ptr::from_ref(code),
                )
            })
        {
            return Err(invalid("direct callee does not match installed code").into());
        }
        if args.len() != callee.arity {
            return Err(VmInternalError::InvalidArgumentCount {
                expected: callee.arity,
                got: args.len(),
            }
            .into());
        }
        let context = self.current_context().clone();
        let actual_caller = self.frame_function_identity(caller_idx);
        let caller_is_observed = matches!(
            &self.frames[caller_idx],
            Frame::Baml(BamlFrame {
                telemetry: Some(_),
                ..
            })
        );
        let frame_telemetry = self.telemetry.as_mut().and_then(|telemetry| {
            // SAFETY: scalar arguments and the callable are live under the permit.
            unsafe {
                telemetry.enter_bytecode(
                    callee,
                    pointer,
                    actual_caller,
                    u32::try_from(caller_site).unwrap_or(u32::MAX),
                    caller_is_observed,
                    args,
                    |caller, callee| Self::register_call_path_functions(&self.heap, caller, callee),
                )
            }
        });
        let locals_offset = StackIndex::from_raw(self.stack.len());
        self.frames.extend(std::iter::once_with(|| {
            Frame::Baml(BamlFrame {
                function: pointer,
                execution: FrameExecution::Active,
                locals_offset,
                type_args: Vec::new(),
                type_metadata: None,
                faulting_pc: 0,
                telemetry: frame_telemetry,
                context,
            })
        }));
        self.cur_pc = 0;
        Ok(())
    }

    fn return_direct(&mut self, result: Value) -> Result<(), VmRustFnError> {
        let index = self.frames.len() - 1;
        let Frame::Baml(frame) = &mut self.frames[index] else {
            unreachable!("generated direct return owns a BAML frame")
        };
        assert!(matches!(frame.execution, FrameExecution::Active));
        let telemetry = frame.telemetry.take();
        // SAFETY: the logical frame roots its function under the active permit.
        let function = unsafe { self.load_function(index)? };
        if let Some(telemetry) = telemetry {
            self.complete_bytecode_invocation_with_function(
                index,
                function,
                telemetry,
                InvocationOutcome::Ok,
                Some(result),
            );
        }
        // The result stays in typed Rust storage; direct calls own no eval slots.
        self.frames.pop();
        let Some(Frame::Baml(caller)) = self.frames.last() else {
            unreachable!("a direct call always has a BAML caller")
        };
        self.cur_pc = caller.faulting_pc;
        let context = self.current_context().clone();
        if let Some(telemetry) = &mut self.telemetry {
            telemetry.set_context(context);
        }
        Ok(())
    }

    fn fail_direct(&mut self, code: &'static CompiledCode, site: usize) {
        let Some(Frame::Baml(frame)) = self.frames.last_mut() else {
            return;
        };
        // SAFETY: the active permit protects the function metadata.
        let Object::Function(function) = (unsafe { frame.function.get() }) else {
            return;
        };
        if function.compiled.as_ref().is_some_and(|installed| {
            std::ptr::eq(
                std::ptr::from_ref::<CompiledCode>(installed),
                std::ptr::from_ref(code),
            )
        }) {
            frame.faulting_pc = site;
            self.cur_pc = site;
        }
    }
}

impl BexVm {
    #[allow(
        clippy::unused_self,
        reason = "the borrow ties the returned array to the active heap context"
    )]
    fn compiled_array(&self, value: Value) -> Result<&bex_vm_types::types::Array, VmInternalError> {
        let invalid = || VmInternalError::InvalidCompiledCode {
            message: "expected int[] in compiled code".into(),
        };
        let pointer = value.as_object_ptr().ok_or_else(invalid)?;
        // SAFETY: compiled values are rooted by the active frame/argument stack.
        // This context cannot outlive the VM's active permit, and array contents
        // are accessed only under their existing container lock.
        #[allow(unsafe_code, reason = "heap access under the active VM permit")]
        let object = unsafe { pointer.get() };
        match object {
            Object::Array(array) if *array.element_ty == RealizedTy::Int => Ok(array),
            _ => Err(invalid()),
        }
    }
}

impl CompiledHeap for BexVm {
    fn check_int_array(&self, value: Value) -> Result<Value, VmInternalError> {
        self.compiled_array(value)?;
        Ok(value)
    }

    fn alloc_int_array(&mut self, values: Vec<Int63>) -> Value {
        Value::object(
            self.tlab.alloc_array(
                RealizedTy::Int,
                values
                    .into_iter()
                    .map(|value| Value::int(value.get()))
                    .collect(),
            ),
        )
    }

    fn int_array_len(&self, array: Value) -> Result<Int63, VmRustFnError> {
        let length = self.compiled_array(array)?.len();
        i64::try_from(length)
            .ok()
            .and_then(Int63::new)
            .ok_or_else(|| {
                VmInternalError::InvalidCompiledCode {
                    message: "array length exceeds int range".into(),
                }
                .into()
            })
    }

    fn int_array_get(&self, array: Value, index: Int63) -> Result<Int63, VmRustFnError> {
        let guard = self.compiled_array(array)?.lock();
        let index = crate::array_index::resolve_index(index.get(), guard.len()).ok_or(
            VmPanic::IndexOutOfBounds {
                index: index.get(),
                length: guard.len(),
            },
        )?;
        Ok(bex_vm_types::compiled::read_int(guard[index])?)
    }

    fn int_array_set(
        &mut self,
        array: Value,
        index: Int63,
        value: Int63,
    ) -> Result<(), VmRustFnError> {
        let mut guard = self.compiled_array(array)?.lock_mut();
        let index = crate::array_index::resolve_index(index.get(), guard.len()).ok_or(
            VmPanic::IndexOutOfBounds {
                index: index.get(),
                length: guard.len(),
            },
        )?;
        let value = Value::int(value.get());
        // This is a no-op for integers, but keeps the existing mutation contract
        // explicit: writes of object references must notify the collector.
        self.heap
            .write_barrier(array.as_object_ptr().expect("validated array"), value);
        guard[index] = value;
        Ok(())
    }
}

impl BexVm {
    /// Initialize once under the active heap permit, then run until the body
    /// returns control. No reference into the heap escapes this interval.
    pub(super) fn run_compiled(
        &mut self,
        frame_idx: &mut usize,
        function: &mut &'static Function,
    ) -> Result<Option<VmExecState>, VmError> {
        let code = function
            .compiled
            .as_ref()
            .ok_or_else(|| invalid("missing compiled descriptor"))?;
        if !function.runtime_package.is_null() {
            return Err(invalid("compiled code cannot execute in a grafted package"));
        }
        let Frame::Baml(frame) = &self.frames[*frame_idx] else {
            unreachable!("BAML dispatcher")
        };
        let base = frame.locals_offset;
        if matches!(frame.execution, FrameExecution::Start) {
            self.cur_pc = 0;
            let end = base
                .raw()
                .checked_add(function.arity)
                .ok_or_else(|| invalid("argument range overflow"))?;
            let args = self
                .stack
                .0
                .get(base.raw()..end)
                .ok_or_else(|| invalid("missing compiled arguments"))?;
            let state = (code.create)(args, self)?;
            let Frame::Baml(frame) = &mut self.frames[*frame_idx] else {
                unreachable!()
            };
            frame.execution = FrameExecution::Compiled(state);
            // The compiled state now owns every live argument. Bytecode's local
            // allocation is no longer a second, unnecessarily retaining root set.
            self.stack.truncate(base.raw());
        }
        // A resumed callee leaves exactly one value at this frame's stack
        // base. A cooperative yield leaves none. The generated continuation
        // validates which input it expects; no second copy of its call state
        // or allocation is needed in the runtime.
        let input = match self.stack.len().checked_sub(base.raw()) {
            Some(0) => ResumeInput::Continue,
            Some(1) => ResumeInput::Returned(self.stack.ensure_pop()),
            _ => return Err(invalid("unexpected compiled operand stack")),
        };
        let Frame::Baml(frame) = &mut self.frames[*frame_idx] else {
            unreachable!()
        };
        let FrameExecution::Compiled(mut active) =
            std::mem::replace(&mut frame.execution, FrameExecution::Active)
        else {
            return Err(invalid("compiled descriptor entered with bytecode state"));
        };
        // The owning Box is held in a Rust local so runtime hooks may grow the
        // logical frame vector. No hook releases the heap permit;
        // restore the state before any GC handoff or exception allocation.
        let owner = *frame_idx;
        // Loan the checker by value while runtime hooks may borrow the VM.
        // Its pressure flags remain shared and its exact counter is restored.
        // The clone costs refcounts once per resume, not a virtual call on every
        // loop edge; keep the generated should_early_yield check inlinable.
        let mut poll = self.early_yield.clone();
        let result = active.resume(input, &mut poll, self);
        self.early_yield = poll;
        let site = active.site();
        let Frame::Baml(frame) = &mut self.frames[owner] else {
            unreachable!("compiled invocation remains live during its body")
        };
        frame.execution = FrameExecution::Compiled(active);
        if site >= code.sites.len() {
            return Err(invalid("compiled source site is out of range"));
        }
        frame.faulting_pc = site;
        if self.frames.len() == owner + 1 {
            self.cur_pc = site;
        } else if result.is_err() {
            // A failing direct chain retains its logical frames and deepest
            // source site for the existing unwinder, after Rust has unwound.
            *frame_idx = self.frames.len() - 1;
            // SAFETY: the active permit still protects every frame's function.
            *function = unsafe { self.load_function(*frame_idx)? };
        } else {
            return Err(invalid(
                "successful compiled body left a direct call active",
            ));
        }
        match result.map_err(|error| self.native_error_to_vm_error(error))? {
            CompiledAction::Yield => Ok(Some(VmExecState::EarlyYield)),
            CompiledAction::Call { target, args } => {
                let slot = *code
                    .calls
                    .get(target)
                    .ok_or_else(|| invalid("compiled call target is out of range"))?;
                let callee =
                    self.load_global_in(function.runtime_package, GlobalIndex::from_raw(slot));
                let callee = self.as_object_ptr(callee, FunctionType::Callable.into())?;
                let count = args.len();
                let locals = StackIndex::from_raw(self.stack.len());
                self.stack.extend(args);
                self.execute_call_from_locals_offset(callee, locals, count, frame_idx, function)
            }
            CompiledAction::Return(value) => {
                self.complete_baml_return(*frame_idx, function, value);
                if self.frames.is_empty() {
                    return Ok(Some(VmExecState::Complete(self.stack.ensure_pop())));
                }
                *frame_idx = self.frames.len() - 1;
                if self.early_yield.should_early_yield() {
                    return Ok(Some(VmExecState::EarlyYield));
                }
                Ok(None)
            }
        }
    }

    /// Common successful completion: telemetry, roots/stack lifetime, context.
    #[inline]
    pub(super) fn complete_baml_return(
        &mut self,
        frame_idx: usize,
        function: &Function,
        result: Value,
    ) {
        let Frame::Baml(frame) = &mut self.frames[frame_idx] else {
            unreachable!("BAML return")
        };
        let base = frame.locals_offset.raw();
        if let Some(telemetry) = frame.telemetry.take() {
            self.complete_bytecode_invocation_with_function(
                frame_idx,
                function,
                telemetry,
                InvocationOutcome::Ok,
                Some(result),
            );
        }
        self.stack.truncate(base);
        self.stack.push(result);
        self.frames.pop();
        // Before another instruction executes, the restored caller is still
        // at its call site. Do not leave the returned callee's coordinate live
        // across a GC yield or a hidden native continuation.
        if let Some(Frame::Baml(caller)) = self
            .frames
            .iter()
            .rev()
            .find(|frame| matches!(frame, Frame::Baml(_)))
        {
            self.cur_pc = caller.faulting_pc;
        }
        let context = self.current_context().clone();
        if let Some(telemetry) = &mut self.telemetry {
            telemetry.set_context(context);
        }
    }
}
