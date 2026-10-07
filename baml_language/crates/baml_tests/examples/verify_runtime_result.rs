//! Check an integer speedtest result before comparing runtime timings.
//! Usage: cargo run -p baml_tests --example verify_runtime_result -- SOURCE EXPECTED
//! Uses the same O2 compiler entry as `runtime_benchmark`; performs no timing.

use std::{path::Path, sync::Arc};

use baml_compiler2_emit::OptLevel;
use baml_db::{ProjectDatabase, compile_program};
use baml_tests::engine::TestDbExt;
use bex_engine::{BexEngine, BexExternalValue, FunctionCallContextBuilder};
use sys_native::{CallId, SysOpsExt};

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    assert_eq!(arguments.len(), 3, "expected SOURCE EXPECTED_INT");
    let source = std::fs::read_to_string(&arguments[1]).expect("read BAML source");
    let expected: i64 = arguments[2].parse().expect("integer oracle");
    let mut db = ProjectDatabase::new();
    let package = db.workspace(Path::new("."));
    db.file("bench.baml", &source);
    let program = compile_program(&db, package, OptLevel::Two).expect("compile benchmark");
    let runtime = tokio::runtime::Runtime::new().expect("Tokio runtime");
    let engine = {
        let _entered = runtime.enter();
        Arc::new(BexEngine::new(program, Arc::new(sys_native::SysOps::native()), vec![]).unwrap())
    };
    let result = runtime
        .block_on(engine.call_function(
            "main",
            vec![],
            FunctionCallContextBuilder::new(CallId::next()).build(),
            true,
        ))
        .expect("execute benchmark");
    assert_eq!(result, BexExternalValue::Int(expected));
    runtime.block_on(engine.shutdown());
    println!("{expected}");
}
