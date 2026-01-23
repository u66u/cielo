use cielo::common::ids::{ExprId, SourceId, StmtId, VarId};
use cielo::common::symbols::Interner;
use cielo::ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, StmtKind, UnaryOp};
use cielo::pipeline::phases::CtPropagationTables;
use cielo::{CompiledC, Compiler, CompilerConfig};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OracleValue {
    Unit,
    Bool(bool),
    Int(i64),
}

struct DiffCase {
    name: &'static str,
    source: &'static str,
}

#[test]
fn runtime_exit_matches_evaluator_oracle_for_pure_seeded_cases() {
    if !c_compiler_available() {
        eprintln!("skipping runtime diff test: no C compiler found");
        return;
    }

    let cases = [
        DiffCase {
            name: "pure_arith",
            source: r#"
fn main() -> Int {
  let x = 4 * 5;
  x + 3
}
"#,
        },
        DiffCase {
            name: "pure_val_if_bool",
            source: r#"
fn main() -> Bool {
  let x = if true { 4 } else { 9 };
  x == 4
}
"#,
        },
    ];

    let compiler = Compiler::new(CompilerConfig::default());
    for (idx, case) in cases.iter().enumerate() {
        let mut interner = Interner::new();
        let compiled = compiler.compile_source_v0_to_c(
            case.source,
            SourceId::from_u32(idx as u32),
            &mut interner,
        );
        let expected = evaluator_oracle_exit_code(&compiled, &interner).unwrap_or_else(|| {
            panic!(
                "failed to compute evaluator oracle for case {}",
                case.name
            )
        });
        let actual = compile_and_run_c_exit_code(case.name, &compiled.c_source);
        assert_eq!(
            actual, expected,
            "runtime/evaluator mismatch for case {}",
            case.name
        );
    }
}

fn evaluator_oracle_exit_code(compiled: &CompiledC, interner: &Interner) -> Option<i32> {
    let program = compiled.residual.program();
    let ct = compiled.residual.ct();
    let main = find_main_body(program, interner)?;

    let mut env = HashMap::new();
    let value = eval_stmt(program, ct, main, &mut env)?;
    oracle_value_to_exit_code(value)
}

fn find_main_body(program: &CoreProgram, interner: &Interner) -> Option<StmtId> {
    program
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .map(|function| function.body)
}

fn eval_stmt(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    stmt_id: StmtId,
    env: &mut HashMap<VarId, OracleValue>,
) -> Option<OracleValue> {
    let stmt = program.stmt(stmt_id)?;
    match &stmt.kind {
        StmtKind::Return(expr) => eval_expr(program, ct, *expr, env),
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            let value = eval_expr(program, ct, *value, env)?;
            env.insert(*binding, value);
            eval_stmt(program, ct, *next, env)
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            let mut value_env = env.clone();
            let value = eval_stmt(program, ct, *value, &mut value_env)?;
            env.insert(*binding, value);
            eval_stmt(program, ct, *next, env)
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => match eval_expr(program, ct, *cond, env)? {
            OracleValue::Bool(true) => {
                let mut then_env = env.clone();
                eval_stmt(program, ct, *then_branch, &mut then_env)
            }
            OracleValue::Bool(false) => {
                let mut else_env = env.clone();
                eval_stmt(program, ct, *else_branch, &mut else_env)
            }
            _ => None,
        },
        StmtKind::Stage { body, next, .. } => {
            let body_value = eval_stmt(program, ct, *body, env)?;
            if let Some(next_stmt) = next {
                eval_stmt(program, ct, *next_stmt, env)
            } else {
                Some(body_value)
            }
        }
        StmtKind::Call { .. }
        | StmtKind::Match { .. }
        | StmtKind::Perform { .. }
        | StmtKind::Resume { .. }
        | StmtKind::Handle { .. }
        | StmtKind::Hole { .. }
        | StmtKind::Error(_) => None,
    }
}

fn eval_expr(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    expr_id: ExprId,
    env: &HashMap<VarId, OracleValue>,
) -> Option<OracleValue> {
    let expr = program.expr(expr_id)?;
    let direct = match &expr.kind {
        ExprKind::Literal(literal) => literal_to_oracle(literal),
        ExprKind::Var(var) => env.get(var).copied(),
        ExprKind::Unary { op, expr } => {
            let value = eval_expr(program, ct, *expr, env)?;
            eval_unary(*op, value)
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let left = eval_expr(program, ct, *lhs, env)?;
            let right = eval_expr(program, ct, *rhs, env)?;
            eval_binary(*op, left, right)
        }
        ExprKind::PureCall { .. }
        | ExprKind::MakeStruct { .. }
        | ExprKind::MakeEnum { .. }
        | ExprKind::Error(_) => None,
    };
    direct.or_else(|| ct.ct_cache.get(&expr_id).and_then(literal_to_oracle))
}

fn eval_unary(op: UnaryOp, value: OracleValue) -> Option<OracleValue> {
    match (op, value) {
        (UnaryOp::Neg, OracleValue::Int(value)) => Some(OracleValue::Int(value.wrapping_neg())),
        (UnaryOp::Not, OracleValue::Bool(value)) => Some(OracleValue::Bool(!value)),
        _ => None,
    }
}

fn eval_binary(op: BinaryOp, lhs: OracleValue, rhs: OracleValue) -> Option<OracleValue> {
    match (op, lhs, rhs) {
        (BinaryOp::Add, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Int(lhs.wrapping_add(rhs)))
        }
        (BinaryOp::Sub, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Int(lhs.wrapping_sub(rhs)))
        }
        (BinaryOp::Mul, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Int(lhs.wrapping_mul(rhs)))
        }
        (BinaryOp::Div, OracleValue::Int(lhs), OracleValue::Int(rhs)) if rhs != 0 => {
            Some(OracleValue::Int(lhs.wrapping_div(rhs)))
        }
        (BinaryOp::Mod, OracleValue::Int(lhs), OracleValue::Int(rhs)) if rhs != 0 => {
            Some(OracleValue::Int(lhs.wrapping_rem(rhs)))
        }
        (BinaryOp::Eq, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Bool(lhs == rhs))
        }
        (BinaryOp::Eq, OracleValue::Bool(lhs), OracleValue::Bool(rhs)) => {
            Some(OracleValue::Bool(lhs == rhs))
        }
        (BinaryOp::Ne, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Bool(lhs != rhs))
        }
        (BinaryOp::Ne, OracleValue::Bool(lhs), OracleValue::Bool(rhs)) => {
            Some(OracleValue::Bool(lhs != rhs))
        }
        (BinaryOp::Lt, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Bool(lhs < rhs))
        }
        (BinaryOp::Le, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Bool(lhs <= rhs))
        }
        (BinaryOp::Gt, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Bool(lhs > rhs))
        }
        (BinaryOp::Ge, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Bool(lhs >= rhs))
        }
        (BinaryOp::And, OracleValue::Bool(lhs), OracleValue::Bool(rhs)) => {
            Some(OracleValue::Bool(lhs && rhs))
        }
        (BinaryOp::Or, OracleValue::Bool(lhs), OracleValue::Bool(rhs)) => {
            Some(OracleValue::Bool(lhs || rhs))
        }
        _ => None,
    }
}

fn literal_to_oracle(literal: &Literal) -> Option<OracleValue> {
    match literal {
        Literal::Unit => Some(OracleValue::Unit),
        Literal::Bool(value) => Some(OracleValue::Bool(*value)),
        Literal::Int(value) => Some(OracleValue::Int(*value)),
        Literal::Float(_) | Literal::Char(_) | Literal::String(_) => None,
    }
}

fn oracle_value_to_exit_code(value: OracleValue) -> Option<i32> {
    let code = match value {
        OracleValue::Unit => 0i32,
        OracleValue::Bool(value) => {
            if value {
                0
            } else {
                1
            }
        }
        OracleValue::Int(value) => value as i32,
    };
    Some((code as u8) as i32)
}

fn c_compiler_available() -> bool {
    Command::new(c_compiler_command())
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn c_compiler_command() -> OsString {
    std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"))
}

fn compile_and_run_c_exit_code(case_name: &str, c_source: &str) -> i32 {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic enough for temp dir naming")
        .as_nanos();
    let work_dir = std::env::temp_dir().join(format!(
        "cielo_runtime_diff_{}_{}_{}",
        case_name,
        std::process::id(),
        stamp
    ));
    fs::create_dir_all(work_dir.as_path()).expect("failed to create temp work dir");
    let c_path = work_dir.join("program.c");
    let bin_path = work_dir.join("program.bin");
    fs::write(c_path.as_path(), c_source).expect("failed to write emitted C source");

    let compile = Command::new(c_compiler_command())
        .arg("-std=c11")
        .arg(c_path.as_path())
        .arg("-o")
        .arg(bin_path.as_path())
        .output()
        .expect("failed to invoke C compiler");
    if !compile.status.success() {
        panic!(
            "C compile failed for case {}:\n{}",
            case_name,
            String::from_utf8_lossy(compile.stderr.as_slice())
        );
    }

    let run = Command::new(bin_path.as_path())
        .output()
        .expect("failed to execute compiled C binary");
    let code = run.status.code().unwrap_or_else(|| {
        panic!(
            "runtime execution terminated by signal for case {}:\nstdout:\n{}\nstderr:\n{}",
            case_name,
            String::from_utf8_lossy(run.stdout.as_slice()),
            String::from_utf8_lossy(run.stderr.as_slice())
        )
    });
    let _ = fs::remove_dir_all(work_dir.as_path());
    code
}
