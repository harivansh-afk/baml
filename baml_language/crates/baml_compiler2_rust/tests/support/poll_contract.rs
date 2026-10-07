use bex_vm::{BexVm, VmExecState};
use bex_vm_types::{EarlyYieldCheck, Program, Value};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn vm(image: &Program, name: &str, flag: Arc<AtomicBool>, argument: i64) -> BexVm {
    let mut program = image.clone();
    let entry = program.rendered_callables()[name].object.raw();
    generated::install(&mut program).unwrap();
    let mut vm = BexVm::from_program(program, flag.clone()).unwrap();
    vm.early_yield = EarlyYieldCheck::with_interval(flag, 16);
    vm.set_entry_point(vm.heap.compile_time_ptr(entry), &[Value::int(argument)]);
    vm
}

fn main() {
    let image: Program = borsh::from_slice(include_bytes!("../program.bin")).unwrap();
    let flag = Arc::new(AtomicBool::new(true));
    let mut running = vm(&image, "user.entry", flag.clone(), 300);
    assert!(matches!(running.exec().unwrap(), VmExecState::EarlyYield));
    assert_eq!(
        generated::DIRECT_ENTRIES.load(Ordering::Relaxed),
        3,
        "the first complete direct chain must spend its bounded work"
    );
    assert_eq!(
        running.frames.len(),
        1,
        "no direct activation may remain at a yield"
    );
    flag.store(false, Ordering::Relaxed);
    assert!(
        matches!(running.exec().unwrap(), VmExecState::Complete(value) if value==Value::int(300))
    );

    let mut failing = vm(&image, "user.errors", Arc::new(AtomicBool::new(true)), 1);
    assert!(failing.exec().is_err());
    assert!(
        failing.early_yield.should_early_yield(),
        "a failing direct region must not lose its polling charge during unwinding"
    );
    println!("poll contract ok");
}
