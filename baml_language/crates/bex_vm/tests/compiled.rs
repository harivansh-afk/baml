//! Execution-contract tests independent of the Rust emitter. Handwritten
//! compiled bodies exercise transitions and GC with deliberately distinct PCs.
#![cfg(not(target_arch = "wasm32"))]

use baml_test_support::compile_source;
use bex_vm::package_baml::{Continuation, NativeCallResult};
use bex_vm::{BexVm, NativeFunction, VmExecState};
use bex_vm_types::{
    EarlyYieldCheck, FunctionKind, HeapPtr, Object, ObjectIndex, Program, RootHaver, Value,
    compiled::*,
    errors::{VmInternalError, VmPanic, VmRustFnError},
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn attach(program: &mut Program, name: &str, create: FrameFactory, sites: usize) -> usize {
    let object = program.rendered_callables()[name].object.raw();
    let Object::Function(function) = &program.objects[ObjectIndex::from_raw(object)] else {
        unreachable!()
    };
    let span = function.span;
    let sites = Box::leak(
        vec![
            CompiledSite {
                file_id: span.file_id.as_u32(),
                start: span.range.start().into(),
                end: span.range.end().into(),
                line: 1,
                kind: SiteKind::Operation
            };
            sites
        ]
        .into_boxed_slice(),
    );
    let code = Box::leak(Box::new(CompiledCode {
        create,
        sites,
        calls: &[],
    }));
    let fingerprint = program_fingerprint(program).unwrap();
    install(program, fingerprint, &[CompiledBinding { object, code }]).unwrap();
    object
}

#[derive(Debug)]
struct Sum {
    remaining: i64,
    total: i64,
}
impl RootHaver for Sum {
    fn collect_roots(&self, _: &mut Vec<HeapPtr>) {}
    fn forward_roots(&mut self, _: &HashMap<HeapPtr, HeapPtr>) {}
}
impl CompiledFrame for Sum {
    fn site(&self) -> usize {
        0
    }
    fn resume(
        &mut self,
        input: ResumeInput,
        poll: &mut EarlyYieldCheck,
        _: &mut dyn CompiledRuntime,
    ) -> Result<CompiledAction, VmRustFnError> {
        assert!(matches!(input, ResumeInput::Continue));
        while self.remaining > 0 {
            if poll.should_early_yield() {
                return Ok(CompiledAction::Yield);
            }
            self.total += self.remaining;
            self.remaining -= 1;
        }
        Ok(CompiledAction::Return(Value::int(self.total)))
    }
}
fn sum(args: &[Value], _: &dyn CompiledHeap) -> Result<Box<dyn CompiledFrame>, VmInternalError> {
    check_arity(args, 1)?;
    Ok(Box::new(Sum {
        remaining: read_int(args[0])?.get(),
        total: 0,
    }))
}

#[test]
fn compiled_bindings_cannot_skip_declared_trace_selection() {
    let mut program = compile_source(
        r#"
        function plain(n: int) -> int { n }
        /// baml:$trace=trace.hidden
        function hooked(n: int) -> int { n }
    "#,
    );
    let plain = attach(&mut program, "user.plain", sum, 1);
    let Object::Function(function) = &program.objects[ObjectIndex::from_raw(plain)] else {
        unreachable!()
    };
    let installed = function.compiled.as_ref().unwrap();
    let code = Box::leak(Box::new(CompiledCode {
        create: installed.create,
        sites: installed.sites,
        calls: installed.calls,
    }));
    let object = program.rendered_callables()["user.hooked"].object.raw();
    let fingerprint = program_fingerprint(&program).unwrap();
    assert!(
        install(
            &mut program,
            fingerprint,
            &[CompiledBinding { object, code }]
        )
        .is_err()
    );
}

#[test]
fn compiled_binding_memory_is_counted_by_the_heap_meter() {
    let mut program = compile_source("function Sum(n: int) -> int { n }");
    let object = program.rendered_callables()["user.Sum"].object;
    let mut before = bex_vm_types::Meter::charge();
    program.objects[object].measure(&mut before);
    attach(&mut program, "user.Sum", sum, 1);
    let mut after = bex_vm_types::Meter::charge();
    program.objects[object].measure(&mut after);
    assert_eq!(
        after.total() - before.total(),
        size_of::<InstalledCode>() + 2 * size_of::<usize>()
    );
}

#[test]
fn compiled_checkpoint_settles_payload_debt_before_polling() {
    let program = compile_source("function entry() -> int { 1 }");
    let flag = Arc::new(AtomicBool::new(false));
    let mut vm = BexVm::from_program(program, flag.clone()).unwrap();
    let mut poll = EarlyYieldCheck::with_interval(flag, 1);
    vm.tlab.alloc_debt().grow(1024);
    assert!(vm.tlab.alloc_debt().balance() > 0);
    assert!(poll.tick());
    assert!(!CompiledRuntime::poll_for_yield(&mut vm, &mut poll));
    assert_eq!(vm.tlab.alloc_debt().balance(), 0);
}

#[test]
fn compiled_loop_parks_and_resumes_live_locals() {
    let mut program = compile_source(
        "function Sum(n: int) -> int { let total = 0; while (n > 0) { total += n; n -= 1; } total }",
    );
    let entry = attach(&mut program, "user.Sum", sum, 1);
    let flag = Arc::new(AtomicBool::new(true));
    let mut vm = BexVm::from_program(program, flag.clone()).unwrap();
    vm.early_yield = EarlyYieldCheck::with_interval(flag.clone(), 3);
    vm.set_entry_point(vm.heap.compile_time_ptr(entry), &[Value::int(100)]);
    assert!(matches!(vm.exec().unwrap(), VmExecState::EarlyYield));
    assert!(matches!(vm.exec().unwrap(), VmExecState::EarlyYield));
    flag.store(false, Ordering::Relaxed);
    assert!(matches!(vm.exec().unwrap(), VmExecState::Complete(v) if v == Value::int(5050)));
}

#[derive(Debug)]
struct Hold {
    value: Value,
    parked: bool,
}
impl RootHaver for Hold {
    fn collect_roots(&self, roots: &mut Vec<HeapPtr>) {
        roots.extend(self.value.as_object_ptr());
    }
    fn forward_roots(&mut self, map: &HashMap<HeapPtr, HeapPtr>) {
        if let Some(ptr) = self.value.as_object_ptr()
            && let Some(&moved) = map.get(&ptr)
        {
            self.value = Value::object(moved);
        }
    }
}
impl CompiledFrame for Hold {
    fn site(&self) -> usize {
        0
    }
    fn resume(
        &mut self,
        _: ResumeInput,
        _: &mut EarlyYieldCheck,
        _: &mut dyn CompiledRuntime,
    ) -> Result<CompiledAction, VmRustFnError> {
        if !std::mem::replace(&mut self.parked, true) {
            Ok(CompiledAction::Yield)
        } else {
            Ok(CompiledAction::Return(self.value))
        }
    }
}
fn hold(args: &[Value], _: &dyn CompiledHeap) -> Result<Box<dyn CompiledFrame>, VmInternalError> {
    check_arity(args, 1)?;
    Ok(Box::new(Hold {
        value: args[0],
        parked: false,
    }))
}

#[test]
#[allow(
    unsafe_code,
    reason = "the only VM is parked while this test runs a real moving collection"
)]
fn moving_gc_rewrites_a_root_owned_only_by_compiled_state() {
    let mut program = compile_source("function Identity(value: string) -> string { value }");
    let entry = attach(&mut program, "user.Identity", hold, 1);
    let mut vm = BexVm::from_program(program, Arc::new(AtomicBool::new(false))).unwrap();
    let original = vm.tlab.alloc_string("retained by compiled state");
    vm.set_entry_point(vm.heap.compile_time_ptr(entry), &[Value::object(original)]);
    assert!(matches!(vm.exec().unwrap(), VmExecState::EarlyYield));
    assert!(
        vm.stack.is_empty(),
        "ownership moved out of the argument stack"
    );
    let mut roots = Vec::new();
    vm.collect_roots(&mut roots);
    assert!(roots.contains(&original));
    // SAFETY: this test owns the only VM, which is parked above; no mutator
    // accesses the heap until forwarding completes.
    let (_, _, forwarded) = unsafe {
        vm.heap
            .collect_garbage_generational(&roots, bex_heap::CollectionLevel::Major)
    };
    vm.forward_roots(&forwarded);
    let VmExecState::Complete(result) = vm.exec().unwrap() else {
        panic!("did not resume")
    };
    assert_ne!(
        result.as_object_ptr().unwrap(),
        original,
        "test must actually move the root"
    );
    assert_eq!(
        vm.as_string(&result).unwrap().as_str(),
        "retained by compiled state"
    );
}

#[test]
fn installed_bindings_survive_clone_but_reject_a_modified_image() {
    let mut program = compile_source("function Sum(n: int) -> int { n }");
    let entry = attach(&mut program, "user.Sum", sum, 1);
    validate_bindings(&program.clone()).unwrap();
    let decoded: Program = borsh::from_slice(&borsh::to_vec(&program).unwrap()).unwrap();
    assert!(
        decoded
            .objects
            .iter()
            .all(|object| !matches!(object, Object::Function(f) if f.compiled.is_some()))
    );
    let Object::Function(function) = &mut program.objects[ObjectIndex::from_raw(entry)] else {
        unreachable!()
    };
    function.name.push_str("_changed");
    assert!(matches!(
        BexVm::from_program(program, Arc::new(AtomicBool::new(false))),
        Err(VmInternalError::InvalidCompiledCode { .. })
    ));
}

#[derive(Debug)]
struct Seven;
impl RootHaver for Seven {
    fn collect_roots(&self, _: &mut Vec<HeapPtr>) {}
    fn forward_roots(&mut self, _: &HashMap<HeapPtr, HeapPtr>) {}
}
impl CompiledFrame for Seven {
    fn site(&self) -> usize {
        150
    }
    fn resume(
        &mut self,
        _: ResumeInput,
        _: &mut EarlyYieldCheck,
        _: &mut dyn CompiledRuntime,
    ) -> Result<CompiledAction, VmRustFnError> {
        Ok(CompiledAction::Return(Value::int(7)))
    }
}
fn seven(args: &[Value], _: &dyn CompiledHeap) -> Result<Box<dyn CompiledFrame>, VmInternalError> {
    check_arity(args, 0)?;
    Ok(Box::new(Seven))
}

fn check_arity(args: &[Value], expected: usize) -> Result<(), VmInternalError> {
    if args.len() != expected {
        return Err(VmInternalError::InvalidArgumentCount {
            expected,
            got: args.len(),
        });
    }
    Ok(())
}

#[test]
fn call_and_return_yields_use_the_active_frames_source_coordinate() {
    let mut program = compile_source(
        "function Seven() -> int { 7 } function Main(n: int) -> int { Seven() + n }",
    );
    let entry = program.rendered_callables()["user.Main"].object.raw();
    attach(&mut program, "user.Seven", seven, 151);
    let flag = Arc::new(AtomicBool::new(true));
    let mut vm = BexVm::from_program(program, flag.clone()).unwrap();
    vm.early_yield = EarlyYieldCheck::with_interval(flag.clone(), 1);
    vm.set_entry_point(vm.heap.compile_time_ptr(entry), &[Value::int(3)]);
    assert!(matches!(vm.exec().unwrap(), VmExecState::EarlyYield));
    assert_eq!(vm.current_function_name().as_deref(), Some("user.Seven"));
    assert_eq!(
        vm.cur_pc, 0,
        "new callee has not executed its first operation"
    );
    assert!(matches!(vm.exec().unwrap(), VmExecState::EarlyYield));
    assert_eq!(vm.current_function_name().as_deref(), Some("user.Main"));
    assert_ne!(
        vm.cur_pc, 150,
        "returned callee's site cannot describe its caller"
    );
    flag.store(false, Ordering::Relaxed);
    assert!(matches!(vm.exec().unwrap(), VmExecState::Complete(value) if value == Value::int(10)));
}
struct ThrowAfter;
impl Continuation for ThrowAfter {
    fn call(self: Box<Self>, _: &mut BexVm, _: Value) -> NativeCallResult {
        NativeCallResult::Error(
            VmPanic::DivisionByZero {
                left: Value::int(1),
                right: Value::int(0),
            }
            .into(),
        )
    }
    fn gc_roots(&self) -> Vec<HeapPtr> {
        vec![]
    }
    fn apply_forwarding(&mut self, _: &HashMap<HeapPtr, HeapPtr>) {}
}
fn invoke(_: &mut BexVm, args: &[Value]) -> NativeCallResult {
    NativeCallResult::YieldToCall {
        callee: args[0].as_object_ptr().unwrap(),
        args: vec![],
        type_args: vec![],
        continuation: Box::new(ThrowAfter),
    }
}

#[test]
fn hidden_continuation_failure_restores_its_interpreted_call_site() {
    let mut program = compile_source(
        r#"
        function Invoke(f: () -> int) -> int { f() }
        function Seven() -> int { 7 }
        function Main() -> int {
            let before = 1;
            { Invoke(Seven) } catch (e) { baml.panics.DivisionByZero => 99 }
        }
    "#,
    );
    let names = program.rendered_callables();
    let entry = names["user.Main"].object.raw();
    let Object::Function(function) = &mut program.objects[names["user.Invoke"].object] else {
        unreachable!()
    };
    function.kind = FunctionKind::Native(invoke as NativeFunction as *const ());
    attach(&mut program, "user.Seven", seven, 151);
    let mut vm = BexVm::from_program(program, Arc::new(AtomicBool::new(false))).unwrap();
    vm.set_entry_point(vm.heap.compile_time_ptr(entry), &[]);
    assert!(matches!(vm.exec().unwrap(), VmExecState::Complete(v) if v == Value::int(99)));
}
