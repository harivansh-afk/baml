//! A raising opcode takes the span of its whole statement and keeps that
//! statement's debugger sequence point, even when its operand is already on
//! the stack and pulling it emits nothing.
use baml_test_support::{OptLevel, compile_source, compile_source_with_opt};
use bex_vm::{BytecodeProgram, convert_program};
use bex_vm_types::{Object, bytecode::OpCode, types::Function};

const SOURCE: &str = r#"
class Failure { code int }
function make_error() -> Failure { Failure { code: 1 } }
function thrower() -> int throws Failure {
    throw make_error()
}
"#;

fn function<'a>(program: &'a BytecodeProgram, name: &str) -> &'a Function {
    program
        .objects
        .iter()
        .find_map(|object| match object {
            Object::Function(function) if function.name == name => Some(function.as_ref()),
            _ => None,
        })
        .expect("function exists")
}

#[test]
fn throw_of_a_call_result_keeps_its_statement_sequence_point() {
    let program = convert_program(compile_source(SOURCE)).unwrap();
    let compact = function(&program, "user.thrower")
        .bytecode
        .compact
        .as_ref()
        .unwrap();
    let mut pc = 0;
    let throw = loop {
        let op = OpCode::try_from(compact.code[pc]).expect("valid opcode");
        if op == OpCode::Throw {
            break pc;
        }
        pc += op.encoded_size();
    };
    let entry = compact
        .line_table
        .iter()
        .rev()
        .find(|entry| entry.pc <= throw)
        .expect("throw has a line entry");
    assert_eq!(entry.pc, throw, "throw starts its own entry");
    assert_eq!(&SOURCE[entry.span.range], "throw make_error()");
    assert!(entry.sequence_point, "throw lost its sequence point");
}

#[test]
fn arithmetic_line_does_not_depend_on_operand_pull_sequence_points() {
    // These expected lines come from the source, not from the native backend
    // or another version of the emitter. Materializing operands and changing
    // optimization level must not change the diagnostic location of division.
    let cases = [
        (
            "function divide(n: int) -> int {\n    let numerator = 10;\n    numerator /\n        n\n}\n",
            3,
        ),
        (
            "function divide(n: int) -> int {\n    10 /\n        n\n}\n",
            2,
        ),
        (
            "function divide(n: int) -> int {\n    let numerator = n + 10;\n    numerator /\n        (n - 1)\n}\n",
            3,
        ),
    ];
    for opt in [OptLevel::Zero, OptLevel::One, OptLevel::Two] {
        for (source, expected_line) in cases {
            let program = convert_program(compile_source_with_opt(source, opt)).unwrap();
            let compact = function(&program, "user.divide")
                .bytecode
                .compact
                .as_ref()
                .unwrap();
            let mut pc = 0;
            let mut saw_division = false;
            while pc < compact.code.len() {
                let op = OpCode::try_from(compact.code[pc]).unwrap();
                if op == OpCode::DivInt {
                    assert_eq!(
                        compact.source_line_for_pc(pc),
                        expected_line,
                        "{opt:?}: {source}"
                    );
                    saw_division = true;
                }
                pc += op.encoded_size();
            }
            assert!(saw_division, "division was not emitted: {opt:?}");
        }
    }
}
