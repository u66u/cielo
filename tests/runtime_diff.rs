use cielo::common::ids::{ExprId, HandlerId, SourceId, StmtId, SymbolId, VarId};
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
    ResumeToken(usize),
}

#[derive(Clone, Debug)]
struct HandlerFrame {
    handler: HandlerId,
    captured_env: HashMap<VarId, OracleValue>,
}

#[derive(Clone, Debug)]
struct Continuation {
    env: HashMap<VarId, OracleValue>,
    handler_stack: Vec<HandlerFrame>,
    next: StmtId,
    result: Option<VarId>,
    used: bool,
}

struct DiffCase {
    name: &'static str,
    source: &'static str,
}

#[test]
fn runtime_exit_matches_evaluator_oracle_for_v1_seeded_cases() {
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
        DiffCase {
            name: "direct_resume_clause",
            source: r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => resume(41)
  }
}
"#,
        },
        DiffCase {
            name: "control_resume_clause",
            source: r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => {
      let y = resume(41);
      y + 1
    }
  }
}
"#,
        },
        DiffCase {
            name: "abortive_clause",
            source: r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  handle { do LocalState.tick(); 9 } with LocalState {
    | tick() => 5
  }
}
"#,
        },
        DiffCase {
            name: "nested_disjoint_handlers",
            source: r#"
effect A { fn ping() -> Int }
effect B { fn pong() -> Int }

fn main() -> Int {
  handle {
    handle {
      do A.ping();
      do B.pong();
      7
    } with A {
      | ping(resume) => resume(1)
    }
  } with B {
    | pong(resume) => resume(2)
  }
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

#[test]
fn scoped_perform_does_not_dispatch_to_wrong_capability() {
    if !c_compiler_available() {
        eprintln!("skipping scoped capability runtime test: no C compiler found");
        return;
    }

    let runtime_header =
        format!("{}/src/backend/cielo_runtime.h", env!("CARGO_MANIFEST_DIR")).replace('\\', "\\\\");
    let c_source = format!(
        r#"
#include <stdint.h>
#include "{runtime_header}"

static int outer_hits = 0;
static int inner_hits = 0;

static CieloValue outer_clause(CieloEvidence* evidence, CieloContinuation* continuation, size_t argc, const CieloValue* args) {{
    (void)evidence;
    (void)continuation;
    (void)argc;
    (void)args;
    outer_hits += 1;
    return cv_unit();
}}

static CieloValue inner_clause(CieloEvidence* evidence, CieloContinuation* continuation, size_t argc, const CieloValue* args) {{
    (void)evidence;
    (void)continuation;
    (void)argc;
    (void)args;
    inner_hits += 1;
    return cv_unit();
}}

int main(void) {{
    CieloClauseEntry outer_entries[1] = {{ {{123u, outer_clause}} }};
    CieloClauseEntry inner_entries[1] = {{ {{123u, inner_clause}} }};

    CieloEvidence outer = {{
        .abi_version = cielo_runtime_abi_version(),
        .effect = 7u,
        .capability_id = 0u,
        .clause_count = 1u,
        .clauses = outer_entries,
        .captures = NULL,
        .reserved0 = NULL,
        .reserved1 = NULL
    }};
    CieloEvidence inner = {{
        .abi_version = cielo_runtime_abi_version(),
        .effect = 7u,
        .capability_id = 0u,
        .clause_count = 1u,
        .clauses = inner_entries,
        .captures = NULL,
        .reserved0 = NULL,
        .reserved1 = NULL
    }};

    uint32_t outer_cap = cielo_handler_push_with_evidence(7u, &outer);
    uint32_t inner_cap = cielo_handler_push_with_evidence(7u, &inner);
    if (outer_cap == 0u || inner_cap == 0u) {{
        return 3;
    }}

    (void)cielo_perform_scoped(7u, inner_cap, 123u, "tick", 0u, NULL);
    cielo_handler_pop(inner_cap);

    (void)cielo_perform_scoped(7u, inner_cap, 123u, "tick", 0u, NULL);
    cielo_handler_pop(outer_cap);

    return (inner_hits == 1 && outer_hits == 0) ? 0 : 1;
}}
"#
    );

    let actual = compile_and_run_c_exit_code("scoped_capability_guard", c_source.as_str());
    assert_eq!(
        actual, 0,
        "scoped perform should not dispatch into a different capability instance"
    );
}

#[test]
fn callback_scoped_capability_avoids_wrong_handler_interception() {
    if !c_compiler_available() {
        eprintln!("skipping callback capability runtime test: no C compiler found");
        return;
    }

    let runtime_header =
        format!("{}/src/backend/cielo_runtime.h", env!("CARGO_MANIFEST_DIR")).replace('\\', "\\\\");
    let c_source = format!(
        r#"
#include <stdint.h>
#include "{runtime_header}"

static int user_hits = 0;
static int internal_hits = 0;

static CieloValue user_clause(CieloEvidence* evidence, CieloContinuation* continuation, size_t argc, const CieloValue* args) {{
    (void)evidence;
    (void)continuation;
    (void)argc;
    (void)args;
    user_hits += 1;
    return cv_unit();
}}

static CieloValue internal_clause(CieloEvidence* evidence, CieloContinuation* continuation, size_t argc, const CieloValue* args) {{
    (void)evidence;
    (void)continuation;
    (void)argc;
    (void)args;
    internal_hits += 1;
    return cv_unit();
}}

static void run_user_callback(uint32_t user_capability) {{
    (void)cielo_perform_scoped(9u, user_capability, 200u, "yield", 0u, NULL);
}}

int main(void) {{
    CieloClauseEntry user_entries[1] = {{ {{200u, user_clause}} }};
    CieloClauseEntry internal_entries[1] = {{ {{200u, internal_clause}} }};

    CieloEvidence user = {{
        .abi_version = cielo_runtime_abi_version(),
        .effect = 9u,
        .capability_id = 0u,
        .clause_count = 1u,
        .clauses = user_entries,
        .captures = NULL,
        .reserved0 = NULL,
        .reserved1 = NULL
    }};
    CieloEvidence internal = {{
        .abi_version = cielo_runtime_abi_version(),
        .effect = 9u,
        .capability_id = 0u,
        .clause_count = 1u,
        .clauses = internal_entries,
        .captures = NULL,
        .reserved0 = NULL,
        .reserved1 = NULL
    }};

    uint32_t user_cap = cielo_handler_push_with_evidence(9u, &user);
    uint32_t internal_cap = cielo_handler_push_with_evidence(9u, &internal);
    if (user_cap == 0u || internal_cap == 0u) {{
        return 3;
    }}

    run_user_callback(user_cap);

    cielo_handler_pop(internal_cap);
    cielo_handler_pop(user_cap);

    return (user_hits == 1 && internal_hits == 0) ? 0 : 1;
}}
"#
    );

    let actual = compile_and_run_c_exit_code("callback_capability_scope", c_source.as_str());
    assert_eq!(
        actual, 0,
        "callback should dispatch to captured capability, not nearest same-effect handler"
    );
}

fn evaluator_oracle_exit_code(compiled: &CompiledC, interner: &Interner) -> Option<i32> {
    let program = compiled.residual.program();
    let ct = compiled.residual.ct();
    let main = find_main_body(program, interner)?;

    let mut env = HashMap::new();
    let mut handler_stack = Vec::new();
    let mut continuations = Vec::new();
    let value = eval_stmt(
        program,
        ct,
        main,
        &mut env,
        &mut handler_stack,
        &mut continuations,
    )?;
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
    handler_stack: &mut Vec<HandlerFrame>,
    continuations: &mut Vec<Continuation>,
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
            eval_stmt(program, ct, *next, env, handler_stack, continuations)
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            let mut value_env = env.clone();
            let value = eval_stmt(
                program,
                ct,
                *value,
                &mut value_env,
                handler_stack,
                continuations,
            )?;
            env.insert(*binding, value);
            eval_stmt(program, ct, *next, env, handler_stack, continuations)
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => match eval_expr(program, ct, *cond, env)? {
            OracleValue::Bool(true) => {
                let mut then_env = env.clone();
                eval_stmt(
                    program,
                    ct,
                    *then_branch,
                    &mut then_env,
                    handler_stack,
                    continuations,
                )
            }
            OracleValue::Bool(false) => {
                let mut else_env = env.clone();
                eval_stmt(
                    program,
                    ct,
                    *else_branch,
                    &mut else_env,
                    handler_stack,
                    continuations,
                )
            }
            _ => None,
        },
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => eval_perform(
            program,
            ct,
            *effect,
            *operation,
            args.as_slice(),
            *result,
            *next,
            env,
            handler_stack,
            continuations,
        ),
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            let arg_value = eval_expr(program, ct, *arg, env)?;
            let resume_id = match env.get(resume).copied()? {
                OracleValue::ResumeToken(id) => id,
                _ => return None,
            };
            let resumed = resume_continuation(
                program,
                ct,
                continuations,
                resume_id,
                arg_value,
            )?;
            env.insert(*result, resumed);
            eval_stmt(program, ct, *next, env, handler_stack, continuations)
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            handler_stack.push(HandlerFrame {
                handler: *handler,
                captured_env: env.clone(),
            });
            let body_value = eval_stmt(program, ct, *body, env, handler_stack, continuations)?;
            handler_stack.pop();

            let handler_def = program.handlers().get(handler.index())?;
            let mut return_env = env.clone();
            return_env.insert(handler_def.return_param, body_value);
            let handled_value = eval_stmt(
                program,
                ct,
                handler_def.return_body,
                &mut return_env,
                handler_stack,
                continuations,
            )?;
            if let Some(next_stmt) = next {
                eval_stmt(program, ct, *next_stmt, env, handler_stack, continuations)
            } else {
                Some(handled_value)
            }
        }
        StmtKind::Stage { body, next, .. } => {
            let body_value = eval_stmt(program, ct, *body, env, handler_stack, continuations)?;
            if let Some(next_stmt) = next {
                eval_stmt(program, ct, *next_stmt, env, handler_stack, continuations)
            } else {
                Some(body_value)
            }
        }
        StmtKind::Call { .. } | StmtKind::Match { .. } | StmtKind::Hole { .. } | StmtKind::Error(_) => None,
    }
}

fn eval_perform(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    effect: cielo::common::ids::EffectLabelId,
    operation: SymbolId,
    args: &[ExprId],
    result: Option<VarId>,
    next: StmtId,
    env: &mut HashMap<VarId, OracleValue>,
    handler_stack: &mut Vec<HandlerFrame>,
    continuations: &mut Vec<Continuation>,
) -> Option<OracleValue> {
    let mut arg_values = Vec::with_capacity(args.len());
    for arg in args {
        arg_values.push(eval_expr(program, ct, *arg, env)?);
    }

    let mut selected = None;
    for (idx, frame) in handler_stack.iter().enumerate().rev() {
        let handler_def = program.handlers().get(frame.handler.index())?;
        if handler_def.effect != effect {
            continue;
        }
        if let Some(clause_idx) = handler_def
            .clauses
            .iter()
            .position(|clause| clause.operation == operation)
        {
            selected = Some((idx, frame.clone(), clause_idx));
            break;
        }
    }

    let (frame_index, frame, clause_index) = selected?;
    let handler_def = program.handlers().get(frame.handler.index())?;
    let clause = handler_def.clauses.get(clause_index)?;
    if clause.params.len() != arg_values.len() {
        return None;
    }

    let continuation_id = continuations.len();
    continuations.push(Continuation {
        env: env.clone(),
        handler_stack: handler_stack.clone(),
        next,
        result,
        used: false,
    });

    let mut clause_env = frame.captured_env;
    for (param, value) in clause.params.iter().zip(arg_values.iter().copied()) {
        clause_env.insert(*param, value);
    }
    if let Some(resume_param) = clause.resume_param {
        clause_env.insert(resume_param, OracleValue::ResumeToken(continuation_id));
    }

    let mut clause_stack = handler_stack[..frame_index].to_vec();
    eval_stmt(
        program,
        ct,
        clause.body,
        &mut clause_env,
        &mut clause_stack,
        continuations,
    )
}

fn resume_continuation(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    continuations: &mut Vec<Continuation>,
    continuation_id: usize,
    arg: OracleValue,
) -> Option<OracleValue> {
    let (next, result, mut env, mut handler_stack) = {
        let continuation = continuations.get_mut(continuation_id)?;
        if continuation.used {
            return None;
        }
        continuation.used = true;
        (
            continuation.next,
            continuation.result,
            continuation.env.clone(),
            continuation.handler_stack.clone(),
        )
    };
    if let Some(result_var) = result {
        env.insert(result_var, arg);
    }
    eval_stmt(
        program,
        ct,
        next,
        &mut env,
        &mut handler_stack,
        continuations,
    )
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
        ExprKind::PureCall { .. } | ExprKind::MakeStruct { .. } | ExprKind::MakeEnum { .. } | ExprKind::Error(_) => None,
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
        (BinaryOp::Eq, OracleValue::Unit, OracleValue::Unit) => Some(OracleValue::Bool(true)),
        (BinaryOp::Ne, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Bool(lhs != rhs))
        }
        (BinaryOp::Ne, OracleValue::Bool(lhs), OracleValue::Bool(rhs)) => {
            Some(OracleValue::Bool(lhs != rhs))
        }
        (BinaryOp::Ne, OracleValue::Unit, OracleValue::Unit) => Some(OracleValue::Bool(false)),
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
        OracleValue::ResumeToken(_) => return None,
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
    let code = run
        .status
        .code()
        .unwrap_or_else(|| {
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
