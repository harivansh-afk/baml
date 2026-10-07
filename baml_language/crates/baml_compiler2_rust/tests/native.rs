//! Every function here runs twice: on the VM through the engine, and as Rust
//! generated from the same MIR. The two must agree on every result and every
//! panic, message included.

use std::{fmt::Write as _, path::Path, process::Command, sync::Arc};

use baml_compiler_diagnostics::Severity;
use baml_compiler2_hir::{item_data::file_functions, loc::FunctionLoc};
use baml_compiler2_mir::OptLevel;
use baml_db::{ProjectDatabase, rust::emit_module};
use baml_test_support as stdlib_prefix;
use bex_engine::{BexCallArg, BexEngine, BexExternalValue, FunctionCallContextBuilder};
use sys_native::SysOpsExt;

#[path = "support/instrument.rs"]
mod instrument;

#[derive(Clone, Copy)]
enum Arg {
    Int(i64),
    Bool(bool),
    Array(&'static [i64]),
}

use Arg::{Array, Bool, Int};

const MAX: i64 = (1 << 62) - 1;
const MIN: i64 = -(1 << 62);

#[tokio::test(flavor = "multi_thread")]
async fn integer_operations() {
    let source = r#"
        function add(a: int, b: int) -> int { a + b }
        function sub(a: int, b: int) -> int { a - b }
        function mul(a: int, b: int) -> int { a * b }
        function div(a: int, b: int) -> int { a / b }
        function rem(a: int, b: int) -> int { a % b }
        function neg(a: int, b: int) -> int { -a }
        function shl(a: int, b: int) -> int { a << b }
        function shr(a: int, b: int) -> int { a >> b }
        function bits(a: int, b: int) -> int { (a & b) ^ (a | 5) }
        function lt(a: int, b: int) -> bool { a < b }
        function ge(a: int, b: int) -> bool { a >= b }
        function eq(a: int, b: int) -> bool { a == b }
    "#;
    let values = [MIN, MIN + 1, -7, -1, 0, 1, 3, 62, 63, MAX];
    let pairs: Vec<Vec<Arg>> = values
        .iter()
        .flat_map(|a| values.iter().map(|b| vec![Int(*a), Int(*b)]))
        .collect();
    let functions = [
        "add", "sub", "mul", "div", "rem", "neg", "shl", "shr", "bits", "lt", "ge", "eq",
    ];
    let cases: Vec<_> = functions.iter().map(|f| (*f, pairs.clone())).collect();
    agree("integer_operations", source, &cases).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn control_flow() {
    let source = r#"
        function nested(n: int) -> int {
            let total = 0;
            let i = 0;
            while (i < n) {
                i += 1;
                let j = 0;
                while (j < 6) {
                    j += 1;
                    if (j == 2) { continue; }
                    if (j == 5) { break; }
                    total += i * 10 + j;
                }
                total += 1000;
            }
            total
        }

        function for_continue(n: int) -> int {
            let total = 0;
            for (let i = 0; i < n; i += 1) {
                if (i % 2 == 0) { continue; }
                if (i > 7) { break; }
                total += i;
            }
            total
        }

        function early_return(n: int) -> int {
            let i = 0;
            while (i < n) {
                let j = 0;
                while (j < n) {
                    if (j == 1) { return 42; }
                    j += 1;
                }
                i += 1;
            }
            1 / (n - n)
        }

        function pick(n: int) -> int {
            match (n) { 1 => 10, 2 => 20, 3 => 30, _ => -1 }
        }

        function guards(n: int) -> int {
            let total = 0;
            if (n != 0 && 10 / n > 1) { total += 1; }
            if (n == 0 || 10 / n > 1) { total += 2; }
            total
        }

        function logic(a: bool, b: bool) -> bool {
            let c = a && b;
            (c || !a) == (b != a)
        }

        function truthy(n: int) -> bool { !n }
    "#;
    let small: Vec<Vec<Arg>> = (-3..=12).map(|n| vec![Int(n)]).collect();
    let bools: Vec<Vec<Arg>> = [(false, false), (false, true), (true, false), (true, true)]
        .iter()
        .map(|(a, b)| vec![Bool(*a), Bool(*b)])
        .collect();
    agree(
        "control_flow",
        source,
        &[
            ("nested", small.clone()),
            ("for_continue", small.clone()),
            ("early_return", small.clone()),
            ("pick", small.clone()),
            ("guards", small.clone()),
            ("logic", bools),
            ("truthy", small),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn calls() {
    let source = r#"
        function step(x: int) -> int { x * 3 + 1 }
        function twice(x: int) -> int { step(step(x)) }
        function left(x: int) -> int { step(x) - 1 }
        function right(x: int) -> int { step(x) + 1 }
        function diamond(x: int) -> int { left(x) * right(x) }
        function pair(a: int, b: int) -> int { a * 10 + b }
        function named(zero: int, big: int) -> int { pair(b = 1 / zero, a = big + 1) }
        function none() -> bool { true }
        function calls_none(n: int) -> bool { none() && n > 0 }
    "#;
    let values: Vec<Vec<Arg>> = [MIN, -5, 0, 4, MAX / 9, MAX]
        .iter()
        .map(|n| vec![Int(*n)])
        .collect();
    agree_with_coverage(
        "calls",
        source,
        &[
            ("twice", values.clone()),
            ("diamond", values.clone()),
            // The second argument is evaluated first, as written.
            (
                "named",
                vec![
                    vec![Int(0), Int(MAX)],
                    vec![Int(1), Int(MAX)],
                    vec![Int(1), Int(1)],
                ],
            ),
            ("none", vec![vec![]]),
            ("calls_none", values),
        ],
        Coverage {
            direct: &["step", "left", "right", "none", "pair"],
            ..Coverage::default()
        },
    )
    .await;
}

#[test]
fn admission_is_per_function_and_recursion_uses_baml_frames() {
    let db = checked_db(
        r#"
        function recurse(n: int) -> int { if (n == 0) { return 0; } recurse(n - 1) }
        function slow(n: int) -> int { let a = ["fallback"]; a.length() + n }
        function calls_slow(n: int) -> int { slow(n) + 1 }
        function text(s: string) -> int { 1 }
        function defaulted(a: int = 2) -> int { a }
        function caught(n: int) -> int { { 1 / n } catch (e) { baml.panics.DivisionByZero => 0 } }
    "#,
    );
    let image = linked(&db);
    let functions = candidates(&db);
    let module = emit_module(&db, &image, &functions).unwrap();
    assert_eq!(module.compiled.len(), 2);
    assert!(module.fallback.iter().any(|f| f.function == "slow"));
    assert!(module.fallback.iter().any(|f| f.function == "caught"));
    assert!(
        !module
            .fallback
            .iter()
            .any(|f| f.function == "calls_slow" || f.function == "recurse")
    );
}

#[test]
fn direct_admission_requires_bounded_scalar_call_regions() {
    let mut source = String::from(
        r#"
        function leaf(x: int) -> int { x + 1 }
        function pair(x: int) -> int { leaf(leaf(x)) }
        function branch(x: int) -> int { if (x > 0) { leaf(x) } else { leaf(-x) } }
        function looped(x: int) -> int { while (x > 0) { x -= 1; } leaf(x) }
        function heap(x: int[]) -> int { leaf(x[0]) }
        function left(x: int) -> int { if (x == 0) { 0 } else { right(x - 1) } }
        function right(x: int) -> int { left(x) }
        function sleeping(x: int) -> int { baml.sys.sleep(baml.time.Duration.from_milliseconds(1n)); x }
        function mixed(x: int) -> int { sleeping(x) }
    "#,
    );
    for i in 0..18 {
        let callee = if i == 0 {
            "leaf".to_owned()
        } else {
            format!("chain{}", i - 1)
        };
        writeln!(source, "function chain{i}(x: int) -> int {{ {callee}(x) }}").unwrap();
    }
    for i in 0..8 {
        let callee = if i == 0 {
            "leaf".to_owned()
        } else {
            format!("wide{}", i - 1)
        };
        writeln!(
            source,
            "function wide{i}(x: int) -> int {{ {callee}(x) + {callee}(x) }}"
        )
        .unwrap();
    }
    let db = checked_db(&source);
    let image = linked(&db);
    let functions = candidates(&db);
    let module = emit_module(&db, &image, &functions).unwrap();
    let direct = |name: &str| {
        module
            .direct_calls
            .iter()
            .find(|f| f.function == name)
            .unwrap()
            .eligible
    };
    for name in ["leaf", "pair", "branch"] {
        assert!(direct(name), "{name}");
    }
    for name in [
        "looped", "heap", "left", "right", "mixed", "chain17", "wide7",
    ] {
        assert!(!direct(name), "{name} must keep cooperative execution");
    }
    let mut reversed = functions;
    reversed.reverse();
    let reversed = emit_module(&db, &image, &reversed).unwrap();
    for entry in &module.direct_calls {
        assert_eq!(
            Some(entry),
            reversed
                .direct_calls
                .iter()
                .find(|f| f.function == entry.function)
        );
    }
    let leaf = image.program.rendered_callables()["user.leaf"].object.raw();
    let direct_body = module
        .source
        .split(&format!("fn direct_{leaf}("))
        .nth(1)
        .unwrap()
        .split("\nstruct Frame")
        .next()
        .unwrap();
    assert!(direct_body.contains("runtime.enter_direct"));
    assert!(!direct_body.contains("CompiledAction::Call"));
    assert!(!direct_body.contains("Box::new"));
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_calls_preserve_stack_limits_and_error_chains() {
    agree_with_coverage(
        "direct_limits",
        r#"
        function leaf(x: int) -> int { 10 / x }
        function middle(x: int) -> int { leaf(x) + 1 }
        function direct(x: int) -> int { middle(x) * 2 }
        function caught(x: int) -> int {
            { direct(x) } catch (e) { baml.panics.DivisionByZero => 99 }
        }
        function descend(n: int) -> int {
            if (n == 0) { middle(1) } else { descend(n - 1) }
        }
    "#,
        &[
            ("direct", vec![vec![Int(0)], vec![Int(2)]]),
            ("caught", vec![vec![Int(0)], vec![Int(2)]]),
            (
                "descend",
                vec![
                    vec![Int(250)],
                    vec![Int(253)],
                    vec![Int(254)],
                    vec![Int(256)],
                ],
            ),
        ],
        Coverage {
            fallback: &["caught"],
            direct: &["leaf", "middle"],
            ..Coverage::default()
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_calls_and_errors() {
    agree_with_coverage(
        "mixed_calls",
        r#"
        function compiled(x: int) -> int { x * 3 }
        function interpreted(x: int) -> int {
            baml.sys.sleep(baml.time.Duration.from_milliseconds(1n));
            let data = [x];
            compiled(data[0])
        }
        function entry(x: int) -> int { interpreted(x) + 1 }
        function recurse(n: int) -> int { if (n == 0) { return 0; } recurse(n - 1) + n }
        function fail(x: int) -> int { 10 / x }
        function literal_min() -> int { -4611686018427387904 }
        function multiline(x: int) -> int {
            let numerator = 10;
            numerator /
                x
        }
        function caught(x: int) -> int {
            { fail(x) } catch (e) { baml.panics.DivisionByZero => 99 }
        }
    "#,
        &[
            ("entry", vec![vec![Int(0)], vec![Int(5)], vec![Int(MAX)]]),
            ("recurse", vec![vec![Int(0)], vec![Int(20)], vec![Int(300)]]),
            ("literal_min", vec![vec![]]),
            ("multiline", vec![vec![Int(0)], vec![Int(2)]]),
            ("caught", vec![vec![Int(0)], vec![Int(2)]]),
            ("fail", vec![vec![Int(0)]]),
        ],
        Coverage {
            fallback: &["interpreted", "caught"],
            native: &["compiled"],
            ..Coverage::default()
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn integer_arrays_share_identity_and_match_errors() {
    let source = r#"
        function make(a: int, b: int) -> int[] { [a, b] }
        function identity(a: int[]) -> int[] { a }
        function copy_edges(a: int[]) -> int[] { [a[-1], a[0]] }
        function equal(a: int[], b: int[]) -> bool { a == b }
        function unequal(a: int[], b: int[]) -> bool { a != b }
        function empty() -> int[] { let a: int[] = []; a }
        function read(a: int[], i: int) -> int { a[i] }
        function write(a: int[], i: int, value: int) -> int { a[i] = value; a[i] }
        function alias(a: int[]) -> int {
            let b = identity(a);
            write(b, 0, 99);
            a[0]
        }
        function rebind(a: int[]) -> int { a = [99]; a[0] }
        function keeps_binding(a: int[]) -> int { rebind(a); a[0] }
        function distinct(n: int) -> int {
            let a = make(n, n + 1);
            let b = make(n, n + 1);
            a[0] += 10;
            b[0]
        }
        function sum(a: int[]) -> int {
            let total = 0;
            for (let i = 0; i < a.length(); i += 1) { total += a[i]; }
            total
        }
        function rhs_first(a: int[], i: int, divisor: int) -> int {
            a[i] = 10 / divisor;
            a[i]
        }
        function returns_alias(n: int) -> int[] {
            let a = make(n, n + 1);
            let b = identity(a);
            b[-1] = 42;
            a
        }
    "#;
    let indices: Vec<Vec<Arg>> = [MIN, -4, -3, -1, 0, 2, 3, MAX]
        .into_iter()
        .map(|i| vec![Array(&[10, 20, 30]), Int(i)])
        .collect();
    let writes: Vec<Vec<Arg>> = indices
        .iter()
        .map(|args| {
            let mut args = args.clone();
            args.push(Int(7));
            args
        })
        .collect();
    agree_with_coverage(
        "integer_arrays",
        source,
        &[
            ("make", vec![vec![Int(MIN), Int(MAX)], vec![Int(1), Int(2)]]),
            ("identity", vec![vec![Array(&[1, 2, 3])], vec![Array(&[])]]),
            ("empty", vec![vec![]]),
            ("copy_edges", vec![vec![Array(&[1, 2, 3])]]),
            (
                "equal",
                vec![
                    vec![Array(&[1, 2]), Array(&[1, 2])],
                    vec![Array(&[1, 2]), Array(&[1, 3])],
                    vec![Array(&[]), Array(&[])],
                ],
            ),
            (
                "unequal",
                vec![
                    vec![Array(&[1, 2]), Array(&[1, 2])],
                    vec![Array(&[1, 2]), Array(&[1, 3])],
                ],
            ),
            ("read", indices),
            ("write", writes),
            ("alias", vec![vec![Array(&[5])]]),
            ("keeps_binding", vec![vec![Array(&[5])]]),
            ("distinct", vec![vec![Int(5)]]),
            (
                "sum",
                vec![
                    vec![Array(&[])],
                    vec![Array(&[1, 2, 3])],
                    vec![Array(&[MAX, 1])],
                ],
            ),
            (
                "rhs_first",
                vec![
                    vec![Array(&[]), Int(0), Int(0)],
                    vec![Array(&[]), Int(0), Int(1)],
                ],
            ),
            ("returns_alias", vec![vec![Int(5)]]),
        ],
        Coverage {
            native: &[
                "make",
                "identity",
                "copy_edges",
                "equal",
                "unequal",
                "empty",
                "read",
                "write",
                "alias",
                "rebind",
                "keeps_binding",
                "distinct",
                "sum",
                "rhs_first",
                "returns_alias",
            ],
            ..Coverage::default()
        },
    )
    .await;
}

#[test]
fn unsupported_heap_types_and_iterators_keep_bytecode() {
    let db = checked_db(
        r#"
        function nested(a: int[][]) -> int { a[0][0] }
        function strings(a: string[]) -> int { a.length() }
        function iterator(a: int[]) -> int { let total = 0; for (let item in a) { total += item; } total }
    "#,
    );
    let image = linked(&db);
    let module = emit_module(&db, &image, &candidates(&db)).unwrap();
    assert!(module.compiled.is_empty());
    assert_eq!(module.fallback.len(), 3);
}

async fn agree(test: &str, source: &str, cases: &[(&str, Vec<Vec<Arg>>)]) {
    agree_with_coverage(test, source, cases, Coverage::default()).await;
}

#[tokio::test]
#[should_panic(expected = "skipped unexpectedly fell back")]
async fn one_native_function_cannot_hide_another_cases_fallback() {
    agree(
        "coverage_guard",
        r#"
        function kept() -> int { 1 }
        function skipped() -> int { let text = "bytecode"; text.length() }
        "#,
        &[("kept", vec![vec![]]), ("skipped", vec![vec![]])],
    )
    .await;
}

/// Every case must enter its native frame unless explicitly listed as fallback.
/// Helpers can additionally require native or specifically direct execution.
#[derive(Default)]
struct Coverage<'a> {
    fallback: &'a [&'a str],
    native: &'a [&'a str],
    direct: &'a [&'a str],
}

async fn agree_with_coverage(
    test: &str,
    source: &str,
    cases: &[(&str, Vec<Vec<Arg>>)],
    coverage: Coverage<'_>,
) {
    let db = checked_db(source);
    let image = linked(&db);
    let module = emit_module(&db, &image, &candidates(&db)).unwrap();
    assert!(
        !module.compiled.is_empty(),
        "test never exercises generated code"
    );
    let callables = image.program.rendered_callables();
    for name in coverage.fallback {
        let object = callables[&format!("user.{name}")].object.raw();
        assert!(
            !module
                .compiled
                .contains(&bex_vm_types::ObjectIndex::from_raw(object)),
            "{name} must exercise bytecode fallback"
        );
    }
    let mut required_entries = Vec::new();
    let required = cases
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !coverage.fallback.contains(name))
        .map(|name| (name, "resume"))
        .chain(coverage.native.iter().map(|name| (*name, "native")))
        .chain(coverage.direct.iter().map(|name| (*name, "direct")));
    for (name, entry) in required {
        let object = callables[&format!("user.{name}")].object.raw();
        assert!(
            module
                .compiled
                .contains(&bex_vm_types::ObjectIndex::from_raw(object)),
            "{name} unexpectedly fell back: {:?}",
            module.fallback
        );
        if entry == "direct" {
            assert!(
                module
                    .direct_calls
                    .iter()
                    .any(|support| support.function == name && support.eligible),
                "{name} lost direct-call admission"
            );
        }
        required_entries.push((object, entry, name));
    }
    let engine = Arc::new(
        BexEngine::new(
            image.program.clone(),
            Arc::new(sys_native::SysOps::native()),
            Vec::new(),
        )
        .unwrap(),
    );
    let mut expected = Vec::new();
    let mut calls = String::new();
    for (name, inputs) in cases {
        for input in inputs {
            let shown = input.iter().map(show_arg).collect::<Vec<_>>().join(", ");
            let label = format!("{name}({shown})");
            let args = input
                .iter()
                .map(|arg| BexCallArg::Provided(Box::new(external(*arg))))
                .collect();
            let context = FunctionCallContextBuilder::new(sys_types::CallId::next()).build();
            let result = engine
                .call_function_bound_args(&format!("user.{name}"), args, context, true)
                .await;
            expected.push(format!("{label} = {}", vm_outcome(result)));
            let args = input
                .iter()
                .map(|arg| match arg {
                    Int(n) => format!("Input::Int({n})"),
                    Bool(b) => format!("Input::Bool({b})"),
                    Array(values) => format!("Input::Array(&{values:?})"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(calls, "(\"user.{name}\", \"{label}\", &[{args}]),");
        }
    }
    let instrumented = instrument::source(&module.source);
    let native = format!(
        "mod generated {{\n{instrumented}\n}}\n{HARNESS}\nstatic CASES: &[(&str, &str, &[Input])] = &[{calls}];\nstatic REQUIRED: &[(usize, &str, &str)] = &{required_entries:?};\n",
    );
    let actual = build_and_run(test, &native, &borsh::to_vec(&image.program).unwrap());
    let actual: Vec<_> = actual.lines().collect();
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual, expected);
    }
}

const HARNESS: &str = r#"
use std::sync::Arc;
use bex_engine::{BexCallArg, BexEngine, BexExternalValue, FunctionCallContextBuilder};
#[derive(Clone, Copy)]
enum Input { Int(i64), Bool(bool), Array(&'static [i64]) }
use sys_native::SysOpsExt;
fn outcome(result: Result<BexExternalValue, bex_engine::EngineError>) -> String {
    match result {
        Ok(BexExternalValue::Int(n)) => format!("int {n}"),
        Ok(BexExternalValue::Bool(b)) => format!("bool {b}"),
        Ok(BexExternalValue::Array { items: values, .. }) => format!("array {values:?}"),
        Ok(other) => panic!("unexpected {other:?}"),
        Err(error) => format!("error {:?}", error.to_string()),
    }
}

#[tokio::main]
async fn main() {
    let mut program = borsh::from_slice(include_bytes!("../program.bin")).unwrap();
    generated::install(&mut program).unwrap();
    let engine = Arc::new(BexEngine::new(program, Arc::new(sys_native::SysOps::native()), Vec::new()).unwrap());
    for (name, label, input) in CASES {
        let args = input.iter().map(|arg| BexCallArg::Provided(Box::new(match *arg {
            Input::Int(n) => BexExternalValue::Int(n),
            Input::Bool(b) => BexExternalValue::Bool(b),
            Input::Array(values) => BexExternalValue::Array {
                element_type: baml_type::RuntimeTy::int(),
                items: values.iter().map(|n| BexExternalValue::Int(*n)).collect(),
            },
        }))).collect();
        println!("{label} = {}", outcome(engine.call_function_bound_args(name, args, FunctionCallContextBuilder::new(sys_types::CallId::next()).build(), true).await));
    }
    let executed = generated::EXECUTIONS.lock().unwrap();
    assert!(!executed.is_empty(), "compiled bodies were never executed");
    for &(object, entry, name) in REQUIRED {
        let reached = match entry {
            "resume" => executed.contains(&(object, false)),
            "direct" => executed.contains(&(object, true)),
            "native" => executed.contains(&(object, false)) || executed.contains(&(object, true)),
            _ => unreachable!("known test entry shape"),
        };
        assert!(reached, "required {entry} entry never executed: {name}");
    }
}
"#;

fn vm_outcome(result: Result<BexExternalValue, bex_engine::EngineError>) -> String {
    match result {
        Ok(BexExternalValue::Int(n)) => format!("int {n}"),
        Ok(BexExternalValue::Bool(b)) => format!("bool {b}"),
        Ok(BexExternalValue::Array { items: values, .. }) => format!("array {values:?}"),
        Ok(other) => panic!("unexpected {other:?}"),
        Err(error) => format!("error {:?}", error.to_string()),
    }
}

fn build_and_run(test: &str, main: &str, program: &[u8]) -> String {
    let release = std::env::var("BAML_NATIVE_TEST_RELEASE").as_deref() == Ok("1");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("src")).unwrap();
    std::fs::write(project.path().join("src/main.rs"), main).unwrap();
    std::fs::write(project.path().join("program.bin"), program).unwrap();
    let mut manifest = format!(
        "[package]\nname = \"rust_backend_{test}\"\nedition = \"2024\"\npublish = false\n[workspace]\n[dependencies]\n"
    );
    for name in [
        "baml_type",
        "bex_vm_types",
        "bex_vm",
        "bex_engine",
        "bex_heap",
        "sys_native",
        "sys_types",
        "btel_types",
        "btel_clock",
        "btel_file",
        "btel_recorder",
        "btel_reader",
    ] {
        let features = if cfg!(feature = "heap_debug") && name == "bex_engine" {
            ", features = [\"heap_debug\"]"
        } else {
            ""
        };
        let _ = writeln!(
            manifest,
            "{name} = {{ path = {:?}{features} }}",
            workspace.join("crates").join(name)
        );
    }
    manifest.push_str("tempfile = \"3\"\n");
    manifest.push_str("borsh = \"1\"\ntokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }\n");
    if release {
        manifest.push_str("[profile.release]\nopt-level = 3\nlto = \"fat\"\ncodegen-units = 1\n");
    }
    std::fs::write(project.path().join("Cargo.toml"), manifest).unwrap();
    std::fs::copy(
        workspace.join("Cargo.lock"),
        project.path().join("Cargo.lock"),
    )
    .unwrap();
    let target = workspace.join("target");
    let mut build = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    build
        .args(["build", "--offline", "--quiet", "--manifest-path"])
        .arg(project.path().join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target);
    if release {
        build.arg("--release");
    }
    let build = build.output().unwrap();
    assert!(
        build.status.success(),
        "generated Rust did not build:\n{}\n{main}",
        String::from_utf8_lossy(&build.stderr)
    );
    let profile = if release { "release" } else { "debug" };
    let run = Command::new(target.join(profile).join(format!(
        "rust_backend_{test}{}",
        std::env::consts::EXE_SUFFIX
    )))
    .env(
        "BAML_TELEMETRY",
        if test == "engine_contract" {
            "high"
        } else {
            "off"
        },
    )
    .output()
    .unwrap();
    assert!(
        run.status.success(),
        "generated program failed:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8(run.stdout).unwrap()
}

fn checked_db(source: &str) -> ProjectDatabase {
    let db = stdlib_prefix::setup_test_db(source);
    let errors: Vec<_> = baml_db::testing::check_user_files(&db)
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message_with_primary_label().into_owned())
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    db
}
fn linked(db: &ProjectDatabase) -> baml_db::program::LinkedProgram {
    baml_db::program::compile_program_with_layout(
        db,
        db.workspace_root().unwrap(),
        OptLevel::One,
        stdlib_prefix::prefix(OptLevel::One),
    )
    .unwrap()
}

// These checks cross the driver/backend boundary. A BAML test cannot supply a
// stale image or a malformed compiler-root map to the Rust emitter.
#[test]
fn driver_rejects_stale_sources_and_invalid_link_metadata() {
    let mut db = checked_db("function value() -> int { 7 }");
    let mut image = linked(&db);
    assert_eq!(
        emit_module(&db, &image, &candidates(&db))
            .unwrap()
            .compiled
            .len(),
        1
    );

    let extra = image.package_roots[0];
    image.package_roots.push(extra);
    assert!(
        emit_module(&db, &image, &candidates(&db))
            .unwrap_err()
            .reason
            .contains("identity map")
    );
    image.package_roots.pop();

    let root = db.workspace_root().unwrap();
    let path = db.workspace_files()[0].path(&db);
    db.add_or_update_file_in(root, &path, "function value() -> int { 8 }");
    assert!(
        emit_module(&db, &image, &candidates(&db))
            .unwrap_err()
            .reason
            .contains("source database")
    );

    let fresh = linked(&db);
    assert_eq!(
        emit_module(&db, &fresh, &candidates(&db))
            .unwrap()
            .compiled
            .len(),
        1
    );
}

fn candidates(db: &ProjectDatabase) -> Vec<FunctionLoc<'_>> {
    db.workspace_files()
        .into_iter()
        .flat_map(|file| file_functions(db, file).iter().copied())
        .collect()
}
fn external(arg: Arg) -> BexExternalValue {
    match arg {
        Int(n) => BexExternalValue::Int(n),
        Bool(b) => BexExternalValue::Bool(b),
        Array(values) => BexExternalValue::Array {
            element_type: baml_type::RuntimeTy::int(),
            items: values.iter().map(|n| BexExternalValue::Int(*n)).collect(),
        },
    }
}
fn show_arg(arg: &Arg) -> String {
    match arg {
        Int(n) => n.to_string(),
        Bool(b) => b.to_string(),
        Array(values) => format!("{values:?}"),
    }
}

#[test]
fn generated_code_preserves_engine_contract() {
    let source = include_str!("support/engine_contract.baml");
    let db = checked_db(source);
    let image = linked(&db);
    let module = emit_module(&db, &image, &candidates(&db)).unwrap();
    for name in ["selected", "hidden_target"] {
        assert!(
            module
                .fallback
                .iter()
                .any(|f| f.function == name && f.reason.contains("declared trace hook")),
            "a native body must not bypass {name}'s trace selection"
        );
    }
    for name in ["selected_caller", "hidden_caller"] {
        let id = image.program.rendered_callables()[&format!("user.{name}")]
            .object
            .raw();
        assert!(
            module
                .compiled
                .contains(&bex_vm_types::ObjectIndex::from_raw(id)),
            "{name} should use native-to-bytecode interop"
        );
    }
    assert!(
        module.compiled.len() >= 7,
        "expected scalar callers/callees, got {:?}",
        module.fallback
    );
    for name in [
        "compiled_leaf",
        "direct_middle",
        "direct_entry",
        "fail",
        "fail_middle",
    ] {
        assert!(
            module
                .direct_calls
                .iter()
                .any(|f| f.function == name && f.eligible),
            "{name} did not get direct code"
        );
    }
    let instrumented = instrument::source(&module.source);
    let native = format!(
        "mod generated {{\n{instrumented}\n}}\nconst BAML: &str = {source:?};\n{}",
        include_str!("support/engine_contract.rs")
    );
    assert_eq!(
        build_and_run(
            "engine_contract",
            &native,
            &borsh::to_vec(&image.program).unwrap()
        ),
        "engine contract ok\n"
    );
}

// The property under test is native admission/dispatch, which BAML itself
// cannot select or inspect. Both backends run in the same telemetry-off child.
#[test]
fn declared_trace_hooks_are_skipped_with_telemetry_off() {
    let db = checked_db(include_str!("support/engine_contract.baml"));
    let image = linked(&db);
    let module = emit_module(&db, &image, &candidates(&db)).unwrap();
    let native = format!(
        "mod generated {{ {} }}\n{}",
        module.source,
        r#"
use std::sync::Arc;
use bex_engine::{BexEngine, BexExternalValue as V, FunctionCallContextBuilder};
use sys_native::SysOpsExt;
#[tokio::main]
async fn main() {
    for native in [false, true] {
        let mut program = borsh::from_slice(include_bytes!("../program.bin")).unwrap();
        if native { generated::install(&mut program).unwrap(); }
        let engine = Arc::new(BexEngine::new(program, Arc::new(sys_native::SysOps::native()), vec![]).unwrap());
        let context = FunctionCallContextBuilder::new(sys_types::CallId::next()).build();
        let result = engine.call_function("user.selected_caller", vec![V::Int(4)], context, true).await.unwrap();
        assert_eq!(result, V::Int(18), "the hook must not mutate its argument with telemetry off");
        engine.shutdown().await;
        assert!(engine.telemetry_result().is_none());
    }
    println!("trace off ok");
}
"#
    );
    assert_eq!(
        build_and_run(
            "trace_off",
            &native,
            &borsh::to_vec(&image.program).unwrap()
        ),
        "trace off ok\n"
    );
}

#[test]
fn generated_arrays_survive_gc_and_mixed_suspension() {
    let db = checked_db(include_str!("support/array_contract.baml"));
    let image = linked(&db);
    let module = emit_module(&db, &image, &candidates(&db)).unwrap();
    let callables = image.program.rendered_callables();
    for name in [
        "allocated_loop",
        "make_array",
        "mutate_array",
        "mixed_array",
        "suspended_array",
        "allocation_loop",
        "increment_slot",
    ] {
        assert!(
            module
                .compiled
                .contains(&callables[&format!("user.{name}")].object),
            "{name} fell back: {:?}",
            module.fallback
        );
    }
    let native = format!(
        "mod generated {{\n{}\n}}\n{}",
        module.source,
        include_str!("support/array_contract.rs")
    );
    assert_eq!(
        build_and_run(
            "array_contract",
            &native,
            &borsh::to_vec(&image.program).unwrap()
        ),
        "array contract ok\n"
    );
}

#[test]
fn direct_regions_charge_polling_without_suspending_direct_frames() {
    let db = checked_db(include_str!("support/poll_contract.baml"));
    let image = linked(&db);
    let module = emit_module(&db, &image, &candidates(&db)).unwrap();
    assert!(module.fallback.is_empty(), "{:?}", module.fallback);
    for name in ["leaf", "middle", "outer", "failing"] {
        assert!(
            module
                .direct_calls
                .iter()
                .any(|f| f.function == name && f.eligible)
        );
    }
    let instrumented = instrument::source(&module.source);
    let main = format!(
        "mod generated {{\n{instrumented}\n}}\n{}",
        include_str!("support/poll_contract.rs"),
    );
    assert_eq!(
        build_and_run(
            "poll_contract",
            &main,
            &borsh::to_vec(&image.program).unwrap()
        ),
        "poll contract ok\n"
    );
}
