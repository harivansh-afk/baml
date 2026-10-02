use bex_engine::{BexEngine, BexExternalValue as External, FunctionCallContextBuilder};
use bex_vm::{BexVm, VmExecState};
use bex_vm_types::{EarlyYieldCheck, Object, Program, RootHaver, Value};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use sys_native::SysOpsExt;

fn context() -> bex_engine::FunctionCallContext {
    FunctionCallContextBuilder::new(sys_types::CallId::next()).build()
}

fn array(values: &[i64]) -> External {
    External::Array {
        element_type: baml_type::RuntimeTy::int(),
        items: values.iter().map(|value| External::Int(*value)).collect(),
    }
}

// This test owns the only VM, which is parked for every raw heap inspection
// and moving collection. It uses the actual generated frame, not a stand-in.
fn generated_roots_move(image: &Program) {
    let mut program = image.clone();
    let entry = program.rendered_callables()["user.allocated_loop"]
        .object
        .raw();
    generated::install(&mut program).unwrap();
    let flag = Arc::new(AtomicBool::new(true));
    let mut vm = BexVm::from_program(program, flag.clone()).unwrap();
    vm.early_yield = EarlyYieldCheck::with_interval(flag.clone(), 2);
    vm.set_entry_point(vm.heap.compile_time_ptr(entry), &[Value::int(5)]);
    assert!(matches!(vm.exec().unwrap(), VmExecState::EarlyYield));
    assert!(
        vm.stack.is_empty(),
        "only generated state may root the array"
    );
    let mut roots = Vec::new();
    vm.collect_roots(&mut roots);
    let original = roots
        .iter()
        .copied()
        .find(|pointer| {
            // SAFETY: the only mutator is parked and no collection has begun.
            matches!(unsafe { pointer.get() }, Object::Array(_))
        })
        .expect("generated state did not expose its allocated array");
    for _ in 0..3 {
        roots.clear();
        vm.collect_roots(&mut roots);
        // SAFETY: the only VM is parked; forward every root before resuming.
        let (_, _, forwarding) = unsafe {
            vm.heap
                .collect_garbage_generational(&roots, bex_heap::CollectionLevel::Major)
        };
        vm.forward_roots(&forwarding);
    }
    flag.store(false, Ordering::Relaxed);
    let VmExecState::Complete(result) = vm.exec().unwrap() else {
        panic!("did not complete")
    };
    let pointer = result.as_object_ptr().unwrap();
    assert_ne!(
        pointer, original,
        "the collection must actually move the array"
    );
    assert_eq!(
        &*vm.as_array(&result).unwrap(),
        &[Value::int(6), Value::int(6)]
    );
}

async fn engine_arrays(image: &Program, compiled: bool) {
    let mut program = image.clone();
    if compiled {
        generated::install(&mut program).unwrap();
    }
    let engine =
        Arc::new(BexEngine::new(program, Arc::new(sys_native::SysOps::native()), vec![]).unwrap());
    for (name, expected) in [
        ("mixed_array", array(&[7, 9])),
        ("spawned_arrays", array(&[7, 6])),
    ] {
        assert_eq!(
            engine
                .call_function(
                    &format!("user.{name}"),
                    vec![External::Int(5)],
                    context(),
                    true
                )
                .await
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        engine
            .call_function(
                "user.concurrent_arrays",
                vec![External::Int(1000)],
                context(),
                true
            )
            .await
            .unwrap(),
        array(&[1000, 1000]),
    );
    let logger = bex_engine::logger::TraceLogger::bounded(4);
    let done = AtomicBool::new(false);
    let call = async {
        let result = engine
            .call_function(
                "user.suspended_array",
                vec![External::Int(5)],
                FunctionCallContextBuilder::new(sys_types::CallId::next())
                    .with_logger(logger.clone())
                    .build(),
                true,
            )
            .await;
        done.store(true, Ordering::SeqCst);
        result.unwrap()
    };
    let collect = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while logger.stats().published == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("suspended array did not reach the checkpoint");
        for _ in 0..3 {
            engine
                .collect_garbage(bex_heap::CollectionLevel::Major)
                .await;
        }
        assert!(
            !done.load(Ordering::SeqCst),
            "GC must complete while the compiled caller is suspended"
        );
    };
    let (result, ()) = tokio::join!(call, collect);
    assert_eq!(result, array(&[7, 6]));
    assert_eq!(
        engine
            .call_function(
                "user.allocation_loop",
                vec![External::Int(100_000)],
                context(),
                true
            )
            .await
            .unwrap(),
        External::Int(199_999)
    );
    engine.shutdown().await;
}

#[tokio::main]
async fn main() {
    let image: Program = borsh::from_slice(include_bytes!("../program.bin")).unwrap();
    generated_roots_move(&image);
    for compiled in [false, true] {
        tokio::time::timeout(Duration::from_secs(20), engine_arrays(&image, compiled))
            .await
            .expect("array execution stalled");
    }
    println!("array contract ok");
}
