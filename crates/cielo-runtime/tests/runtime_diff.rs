use cielo_base::Interner;
use cielo_base::{ExprId, HandlerId, SourceId, StmtId, SymbolId, VarId};
use cielo_ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, StmtKind, UnaryOp};
use cielo_memory::MemoryPreset;
use cielo_sema::SemanticTables;
use cielo_staging::pipeline::phases::CtPropagationTables;
use cielo_test_support::{CompiledC, PassConfig, PassHarness};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, PartialEq, Eq, Debug)]
enum OracleValue {
    Unit,
    Bool(bool),
    Int(i64),
    ResumeToken(usize),
    Ctor {
        variant: SymbolId,
        fields: Vec<OracleValue>,
    },
}

/// Guards against a non-terminating program taking the test process with it.
const ORACLE_MAX_CALL_DEPTH: usize = 256;

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

struct GeneratedDiffCase {
    name: String,
    source: String,
    seed: u64,
    template: &'static str,
}

struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        let initial = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state: initial }
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_bool(&mut self) -> bool {
        (self.next_u64() & 1) == 1
    }

    fn next_bounded_u64(&mut self, upper_exclusive: u64) -> u64 {
        if upper_exclusive == 0 {
            0
        } else {
            self.next_u64() % upper_exclusive
        }
    }

    fn next_small_int(&mut self, upper_exclusive: i64) -> i64 {
        self.next_bounded_u64(upper_exclusive as u64) as i64
    }
}

fn bool_lit(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// Depth is the only termination condition, so every recursive arm must
/// decrement it and the leaf arms must be reachable at depth 0.
struct ExprGen<'a> {
    rng: &'a mut DeterministicRng,
    ints: Vec<String>,
    bools: Vec<String>,
}

impl ExprGen<'_> {
    fn int_expr(&mut self, depth: u32) -> String {
        if depth == 0 {
            return self.int_leaf();
        }
        match self.rng.next_bounded_u64(6) {
            0 => self.int_leaf(),
            1 => format!("(0 - {})", self.int_expr(depth - 1)),
            // Magnitudes stay small so overflow is rare rather than impossible:
            // an overflowing case traps in C and the oracle declines, which
            // silently drops it, so widening the leaves would buy coverage of
            // nothing while shrinking the set of cases actually compared.
            2 => {
                let (lhs, rhs) = (self.int_expr(depth - 1), self.int_expr(depth - 1));
                let op = ["+", "-", "*"][self.rng.next_bounded_u64(3) as usize];
                format!("({lhs} {op} {rhs})")
            }
            // A generated divisor is shifted away from zero: a trapping divide
            // makes the oracle decline to judge, which silently drops the case.
            3 => {
                let lhs = self.int_expr(depth - 1);
                let rhs = self.int_expr(depth - 1);
                let op = ["/", "%"][self.rng.next_bounded_u64(2) as usize];
                format!("({lhs} {op} (({rhs} % 7) + 8))")
            }
            _ => self.int_leaf(),
        }
    }

    fn bool_expr(&mut self, depth: u32) -> String {
        if depth == 0 {
            return self.bool_leaf();
        }
        match self.rng.next_bounded_u64(5) {
            0 => self.bool_leaf(),
            1 => format!("(!{})", self.bool_expr(depth - 1)),
            2 => {
                let (lhs, rhs) = (self.int_expr(depth - 1), self.int_expr(depth - 1));
                let op = ["==", "!=", "<", ">", "<=", ">="][self.rng.next_bounded_u64(6) as usize];
                format!("({lhs} {op} {rhs})")
            }
            3 => {
                let (lhs, rhs) = (self.bool_expr(depth - 1), self.bool_expr(depth - 1));
                let op = ["&&", "||"][self.rng.next_bounded_u64(2) as usize];
                format!("({lhs} {op} {rhs})")
            }
            _ => {
                let (lhs, rhs) = (self.bool_expr(depth - 1), self.bool_expr(depth - 1));
                format!("({lhs} == {rhs})")
            }
        }
    }

    fn int_leaf(&mut self) -> String {
        if !self.ints.is_empty() && self.rng.next_bool() {
            let index = self.rng.next_bounded_u64(self.ints.len() as u64) as usize;
            return self.ints[index].clone();
        }
        format!("{}", self.rng.next_small_int(40) - 20)
    }

    fn bool_leaf(&mut self) -> String {
        if !self.bools.is_empty() && self.rng.next_bool() {
            let index = self.rng.next_bounded_u64(self.bools.len() as u64) as usize;
            return self.bools[index].clone();
        }
        bool_lit(self.rng.next_bool()).to_owned()
    }
}

/// Nested arithmetic, comparison and boolean expressions over a `let` chain.
///
/// This exercises the divergence class the oracle can actually judge: anything
/// where Core evaluation and the compiled C disagree. It cannot catch a rule
/// both sides implement identically wrongly -- see `eval_binary`.
fn build_generated_expr_source(rng: &mut DeterministicRng) -> String {
    let bindings = 2 + rng.next_bounded_u64(3) as usize;
    let mut builder = ExprGen {
        rng,
        ints: Vec::new(),
        bools: Vec::new(),
    };
    let mut body = String::new();
    for index in 0..bindings {
        let name = format!("v{index}");
        if builder.rng.next_bool() {
            let value = if builder.rng.next_bool() {
                let cond = builder.bool_expr(1);
                let then_branch = builder.int_expr(2);
                let else_branch = builder.int_expr(2);
                format!("if {cond} {{ {then_branch} }} else {{ {else_branch} }}")
            } else {
                builder.int_expr(2)
            };
            body.push_str(&format!("  let {name} = {value};\n"));
            builder.ints.push(name);
        } else {
            let value = builder.bool_expr(2);
            body.push_str(&format!("  let {name} = {value};\n"));
            builder.bools.push(name);
        }
    }
    let tail = builder.int_expr(3);
    // Exit codes are the low 8 bits, so fold into a range the harness can read
    // back unambiguously from both sides.
    format!("fn main() -> Int {{\n{body}  (({tail}) % 100) + 100\n}}\n")
}

fn build_generated_expr_case(seed: u64, case_index: usize) -> GeneratedDiffCase {
    let mut rng = DeterministicRng::new(seed);
    GeneratedDiffCase {
        name: format!("generated_expr_{case_index:02}"),
        source: build_generated_expr_source(&mut rng),
        seed,
        template: "expr",
    }
}

fn build_generated_handler_case(seed: u64, case_index: usize) -> GeneratedDiffCase {
    let mut rng = DeterministicRng::new(seed);
    let template_kind = rng.next_bounded_u64(4);
    let (template, source) = match template_kind {
        0 => ("abortive", build_abortive_handler_source(&mut rng)),
        1 => ("direct_resume", build_direct_handler_source(&mut rng)),
        2 => ("control_resume", build_control_handler_source(&mut rng)),
        _ => ("nested_same_effect", build_nested_handler_source(&mut rng)),
    };
    GeneratedDiffCase {
        name: format!("generated_handler_{case_index:02}_{template}"),
        source,
        seed,
        template,
    }
}

fn build_abortive_handler_source(rng: &mut DeterministicRng) -> String {
    let perform_flag = bool_lit(rng.next_bool());
    let guard_flag = bool_lit(rng.next_bool());
    let base = 1 + rng.next_small_int(25);
    let then_delta = 1 + rng.next_small_int(12);
    let else_delta = 1 + rng.next_small_int(12);
    let clause_true = 1 + rng.next_small_int(40);
    let clause_false = 1 + rng.next_small_int(40);
    let tail = rng.next_small_int(8);
    format!(
        r#"
effect LocalState {{ fn tick(flag: Bool) -> Int }}

fn main() -> Int {{
  let base = {base};
  let out = handle {{
    do LocalState.tick({perform_flag});
    if {guard_flag} {{
      base + {then_delta}
    }} else {{
      base + {else_delta}
    }}
  }} with LocalState {{
    | tick(flag) => if flag {{ {clause_true} }} else {{ {clause_false} }}
  }};
  out + {tail}
}}
"#
    )
}

fn build_direct_handler_source(rng: &mut DeterministicRng) -> String {
    let perform_flag = bool_lit(rng.next_bool());
    let guard_flag = bool_lit(rng.next_bool());
    let base = 1 + rng.next_small_int(25);
    let then_delta = 1 + rng.next_small_int(12);
    let else_delta = 1 + rng.next_small_int(12);
    let resume_true = 1 + rng.next_small_int(30);
    let resume_false = 1 + rng.next_small_int(30);
    let tail = rng.next_small_int(8);
    format!(
        r#"
effect LocalState {{ fn tick(flag: Bool) -> Int }}

fn main() -> Int {{
  let base = {base};
  let out = handle {{
    do LocalState.tick({perform_flag});
    if {guard_flag} {{
      base + {then_delta}
    }} else {{
      base + {else_delta}
    }}
  }} with LocalState {{
    | tick(flag, resume) => if flag {{ resume({resume_true}) }} else {{ resume({resume_false}) }}
  }};
  out + {tail}
}}
"#
    )
}

fn build_control_handler_source(rng: &mut DeterministicRng) -> String {
    let perform_flag = bool_lit(rng.next_bool());
    let guard_flag = bool_lit(rng.next_bool());
    let base = 1 + rng.next_small_int(25);
    let then_delta = 1 + rng.next_small_int(12);
    let else_delta = 1 + rng.next_small_int(12);
    let resume_true = 1 + rng.next_small_int(30);
    let resume_false = 1 + rng.next_small_int(30);
    let clause_bonus = 1 + rng.next_small_int(7);
    let tail = rng.next_small_int(8);
    format!(
        r#"
effect LocalState {{ fn tick(flag: Bool) -> Int }}

fn main() -> Int {{
  let base = {base};
  let out = handle {{
    do LocalState.tick({perform_flag});
    if {guard_flag} {{
      base + {then_delta}
    }} else {{
      base + {else_delta}
    }}
  }} with LocalState {{
    | tick(flag, resume) => {{
      let resumed = if flag {{
        resume({resume_true})
      }} else {{
        resume({resume_false})
      }};
      resumed + {clause_bonus}
    }}
  }};
  out + {tail}
}}
"#
    )
}

fn build_nested_handler_source(rng: &mut DeterministicRng) -> String {
    let inner_flag = bool_lit(rng.next_bool());
    let outer_flag = bool_lit(rng.next_bool());
    let inner_tail = 1 + rng.next_small_int(20);
    let inner_resume_true = 1 + rng.next_small_int(25);
    let inner_resume_false = 1 + rng.next_small_int(25);
    let outer_resume_true = 1 + rng.next_small_int(25);
    let outer_resume_false = 1 + rng.next_small_int(25);
    let outer_add = 1 + rng.next_small_int(10);
    let tail = rng.next_small_int(8);
    format!(
        r#"
effect LocalState {{ fn tick(flag: Bool) -> Int }}

fn main() -> Int {{
  let out = handle {{
    let inner = handle {{
      do LocalState.tick({inner_flag});
      {inner_tail}
    }} with LocalState {{
      | tick(flag, resume) => if flag {{ resume({inner_resume_true}) }} else {{ resume({inner_resume_false}) }}
    }};
    do LocalState.tick({outer_flag});
    inner + {outer_add}
  }} with LocalState {{
    | tick(flag, resume) => if flag {{ resume({outer_resume_true}) }} else {{ resume({outer_resume_false}) }}
  }};
  out + {tail}
}}
"#
    )
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
        DiffCase {
            name: "deep_disjoint_handler_stack",
            source: r#"
effect A { fn ping() -> Int }
effect B { fn pong() -> Int }
effect C { fn zap() -> Int }

fn main() -> Int {
  handle {
    handle {
      handle {
        do A.ping();
        do B.pong();
        do C.zap();
        7
      } with C {
        | zap(resume) => resume(0)
      }
    } with B {
      | pong(resume) => resume(0)
    }
  } with A {
    | ping(resume) => resume(0)
  }
}
"#,
        },
        DiffCase {
            name: "deep_overlapping_same_effect_stack",
            source: r#"
effect A { fn ping() -> Int }

fn main() -> Int {
  handle {
    handle {
      handle {
        do A.ping();
        7
      } with A {
        | ping() => 11
      }
    } with A {
      | ping() => 22
    }
  } with A {
    | ping() => 33
  }
}
"#,
        },
        DiffCase {
            name: "merged_tail_resume_sites",
            source: r#"
effect St { fn tick(n: Int) -> Int }

fn body(x: Int) -> Int with St {
  let a = do St.tick(1);
  let b = do St.tick(0);
  let c = do St.tick(2);
  x + a + b + c
}

fn main() -> Int {
  handle { body(7) } with St {
    | tick(n, resume) => if n > 1 { resume(100) } else { if n > 0 { resume(10) } else { resume(20) } }
  }
}
"#,
        },
        DiffCase {
            // Two resume sites, neither in tail position. One shared copy of the
            // continuation serves both, and the `Switch` at the end of it has to
            // send each perform back to the arm belonging to the site that
            // entered it -- picking the other arm still runs, and still returns
            // an integer (CIELO-39).
            name: "defunctionalised_resume_sites",
            source: r#"
effect St { fn tick(n: Int) -> Int }

fn main() -> Int {
  handle {
    let a = do St.tick(1);
    let b = do St.tick(0);
    7 + a + b
  } with St {
    | tick(n, resume) => if n > 0 { let y = resume(10); y + 1 } else { let z = resume(20); z * 2 }
  }
}
"#,
        },
        DiffCase {
            // `g` is bound on one site's path only and read after that site, so
            // one shared continuation would leave the other site's arm reading
            // it undefined. The clause has to stay inlined; before the sharing
            // condition looked past `Resume`'s own continuation it did not, and
            // the CFG failed its definedness check (CIELO-39).
            name: "resume_site_carrying_a_branch_local",
            source: r#"
effect St { fn tick(n: Int) -> Int }

fn main() -> Int {
  handle {
    let a = do St.tick(1);
    let b = do St.tick(0);
    7 + a + b
  } with St {
    | tick(n, resume) => if n > 0 { let g = n * 3; let y = resume(g); y + g } else { let z = resume(20); z * 2 }
  }
}
"#,
        },
        DiffCase {
            // The same shape with the sites reached through a nested match, so
            // the dispatch has three arms and the clause branches on a value
            // bound before the split.
            name: "defunctionalised_resume_sites_three_way",
            source: r#"
effect St { fn tick(n: Int) -> Int }

fn main() -> Int {
  handle {
    let a = do St.tick(2);
    let b = do St.tick(1);
    let c = do St.tick(0);
    a + b + c
  } with St {
    | tick(n, resume) => if n > 1 {
        let x = resume(100);
        x + n
      } else {
        if n > 0 { let y = resume(10); y * 2 } else { let z = resume(1); z - 3 }
      }
  }
}
"#,
        },
        DiffCase {
            // Two enums declare `Empty`, so the variant tags collide. Both the
            // oracle and the emitted C dispatch on the tag alone, which is only
            // sound because each value reaches a match on its own enum.
            name: "shared_variant_name_across_enums",
            source: r#"
enum Shape { Empty, Circle(Int) }
enum Buffer { Empty, Full(Int) }

fn shape_code(s: Shape) -> Int {
  match s {
    | Empty => 1
    | Circle(r) => r
  }
}

fn buffer_code(b: Buffer) -> Int {
  match b {
    | Empty => 10
    | Full(n) => n
  }
}

fn empty_buffer() -> Buffer { Empty }

fn main() -> Int {
  let s: Shape = Empty;
  shape_code(s) + buffer_code(empty_buffer()) + shape_code(Circle(2)) + buffer_code(Full(3))
}
"#,
        },
        DiffCase {
            // `double` is called only from a clause body. When reachability
            // skipped clause bodies it was pruned, and the stale callee id
            // aliased `main`.
            name: "clause_body_calls_helper",
            source: r#"
effect St { fn tick(n: Int) -> Int }

fn double(x: Int) -> Int { x + x }

fn main() -> Int {
  let x = handle {
    let v = do St.tick(3);
    v
  } with St {
    | tick(n, resume) => if n > 0 { resume(double(n)) } else { resume(0) }
  };
  x
}
"#,
        },
        DiffCase {
            name: "abortive_clause_calls_helper",
            source: r#"
effect St { fn tick(n: Int) -> Int }

fn triple(x: Int) -> Int { x + x + x }

fn main() -> Int {
  let x = handle {
    let v = do St.tick(3);
    v
  } with St {
    | tick(n) => triple(n)
  };
  x
}
"#,
        },
        DiffCase {
            // Only the inner `St` clause performs `Log`, so the outer handler's
            // body row does not mention it as written. Eliminating `Log` as dead
            // left the perform that clause inlining splices in with no handler.
            name: "clause_body_performs_outer_effect",
            source: r#"
effect St { fn tick(n: Int) -> Int }
effect Log { fn emit(n: Int) -> Int }

fn main() -> Int {
  let r = handle {
    let inner = handle {
      let a = do St.tick(1);
      a
    } with St {
      | tick(n) => { let e = do Log.emit(n); e }
    };
    inner
  } with Log {
    | emit(m, resume) => resume(m + 40)
  };
  r
}
"#,
        },
        DiffCase {
            // The inner clause performs `Log` *before* it resumes, so the `Log`
            // clause is spliced in ahead of the `St` resume. Dropping the outer
            // clause context there left that resume with nothing naming its
            // continuation, and the program did not lower at all (CIELO-54).
            name: "clause_performs_before_resuming",
            source: r#"
effect St { fn tick(n: Int) -> Int }
effect Log { fn emit(n: Int) -> Int }

fn main() -> Int {
  let r = handle {
    let inner = handle {
      do St.tick(1);
      do St.tick(2);
      7
    } with St {
      | tick(n, resume) => if n > 0 { let e = do Log.emit(n); resume(e) } else { resume(2) }
    };
    inner
  } with Log {
    | emit(m, resume) => resume(m)
  };
  r
}
"#,
        },
        DiffCase {
            // Same shape reached through a call, so `St` is specialized into the
            // callee instead of inlined. The call kept the unspecialized row, so
            // `Log` looked dead in `main` and the perform trapped at runtime.
            name: "specialized_clause_performs_outer_effect",
            source: r#"
effect St { fn tick(n: Int) -> Int }
effect Log { fn emit(n: Int) -> Int }

fn helper(x: Int) -> Int with St {
  let a = do St.tick(x);
  a
}

fn main() -> Int {
  let r = handle {
    let inner = handle {
      helper(1)
    } with St {
      | tick(n, resume) => { let e = do Log.emit(n); resume(e) }
    };
    inner
  } with Log {
    | emit(m, resume) => resume(m + 6)
  };
  r
}
"#,
        },
        DiffCase {
            name: "discharged_handler_rt_passthrough",
            source: r#"
effect LocalState { fn tick() -> Int }

fn main() -> Int {
  let rt_input = @runtime { 5 };
  let out = handle {
    do LocalState.tick();
    rt_input + 3
  } with LocalState {
    | tick(resume) => resume(0)
  };
  out
}
"#,
        },
    ];

    let compiler = PassHarness::new(PassConfig::default());
    for (idx, case) in cases.iter().enumerate() {
        let mut interner = Interner::new();
        let compiled = compiler.compile_source_to_c(
            case.source,
            SourceId::from_u32(idx as u32),
            &mut interner,
        );
        let expected = evaluator_oracle_exit_code(&compiled, &interner)
            .unwrap_or_else(|| panic!("failed to compute evaluator oracle for case {}", case.name));
        let actual = compile_and_run_c_exit_code(case.name, &compiled.c_source);
        assert_eq!(
            actual, expected,
            "runtime/evaluator mismatch for case {}",
            case.name
        );
    }
}

/// The memory strategy must not change what a program computes. This is the
/// check that catches an ARC pass which moves a value it does not own: the
/// refcounts stay balanced and ASan stays quiet, only the answer differs.
#[test]
fn every_memory_preset_produces_the_same_exit_code() {
    if !c_compiler_available() {
        eprintln!("skipping memory preset differential: no C compiler found");
        return;
    }

    let cases = [
        (
            "shared_ctor_read_twice",
            r#"
enum Wrap { W(Int) }
fn take(w: Wrap) -> Int {
  match w {
    W(x) => x
  }
}
fn main() -> Int {
  let a = W(1);
  take(a) + take(a)
}
"#,
        ),
        (
            "nested_ctor_projection",
            r#"
struct Pair { a: Int, b: Int }
fn main() -> Int {
  let p = Pair(10, 32);
  p.a + p.b
}
"#,
        ),
        (
            // `q` is live only on the else path. Before edge drops existed it
            // was never released on the taken path: ASan reported 64 bytes.
            "value_live_on_one_branch_only",
            r#"
enum Box { B(Int) }
fn pick(flag: Bool, x: Box, y: Box) -> Int {
  if flag {
    match x { B(a) => a }
  } else {
    match y { B(b) => b }
  }
}
fn main() -> Int {
  let p = B(3);
  let q = B(4);
  pick(true, p, q)
}
"#,
        ),
        (
            // `boxed` joins two fresh allocations and is read once, so the ARC
            // pass emits `cielo_ctor_take_field_unique` and skips the runtime
            // `rc == 1` gate. `-DCIELO_ARC_STATS` arms the assertion inside
            // that helper, so an unsound Unique answer traps here.
            "statically_unique_take",
            r#"
enum Leaf { N(Int) }
enum Boxed { Wrap(Leaf), Empty(Int) }
fn peel(l: Leaf) -> Int {
  match l { | N(v) => v | _ => 0 }
}
fn main() -> Int {
  let seed = @runtime { 1 + 2 };
  let boxed = if seed > 2 { Wrap(N(seed)) } else { Wrap(N(0)) };
  match boxed { | Wrap(leaf) => peel(leaf) | _ => 0 }
}
"#,
        ),
        (
            // Specialization inlines `mk` into a constructor sitting directly
            // under the projection. `cielo_ctor_field_copy` does not release its
            // base, and before the constructor was bound to a value there was no
            // name for a release to mention: ASan reported 80 bytes.
            "ctor_produced_in_expression_position",
            r#"
struct Pair { a: Int, b: Int }
fn mk(x: Int) -> Pair { Pair(x, 2) }
fn main() -> Int {
  let v = mk(1).a;
  v + 2
}
"#,
        ),
        (
            // Same shape, but the parent is read twice, so the take must keep
            // its runtime gate. This is the CIELO-3 regression in the presence
            // of the new unchecked path.
            "shared_parent_keeps_its_gate",
            r#"
enum Leaf { N(Int) }
enum Boxed { Wrap(Leaf), Empty(Int) }
fn peel(b: Boxed) -> Int {
  match b { | Wrap(l) => match l { | N(v) => v | _ => 0 } | _ => 0 }
}
fn main() -> Int {
  let seed = @runtime { 1 + 2 };
  let boxed = if seed > 2 { Wrap(N(seed)) } else { Wrap(N(0)) };
  peel(boxed) + peel(boxed)
}
"#,
        ),
        (
            // Returning a binder makes the arm edge carry a drop, so ARC
            // interposes a block on it. With parameters of its own that block
            // wrote unit over the payload: 0 instead of 7, under ARC only.
            "match_binder_returned_across_an_edge_drop",
            r#"
enum OptInt { SomeI(Int), NoneI }
enum OptOpt { SomeO(OptInt), NoneO }
fn unwrap_o(opt: OptOpt, fallback: OptInt) -> OptInt {
  match opt { | SomeO(v) => v | _ => fallback }
}
fn main() -> Int {
  let back = unwrap_o(SomeO(SomeI(7)), NoneI());
  match back { | SomeI(n) => n | _ => 5 }
}
"#,
        ),
        (
            // Same shape one level out: the concatenated string is an operand of
            // `==`, and `cv_eq` releases nothing.
            "string_builtin_in_expression_position",
            r#"
fn main() -> Int {
  let same = str_concat("ab", "c") == "abc";
  if same { 3 } else { 4 }
}
"#,
        ),
        (
            // A producer in the scrutinee is read, not consumed, so it needs a
            // name too, and the arm still moves fields out of it.
            "ctor_produced_in_scrutinee_position",
            r#"
enum Box { B(Int) }
fn mk(x: Int) -> Box { B(x) }
fn main() -> Int {
  match mk(6) {
    B(x) => x
  }
}
"#,
        ),
        (
            // Same variant name in two enums: the payload of one must never be
            // released against the layout of the other.
            "shared_variant_name_across_enums",
            r#"
enum Shape { Empty, Circle(Int) }
enum Buffer { Empty, Full(Int) }
fn shape_code(s: Shape) -> Int {
  match s { | Empty => 1 | Circle(r) => r }
}
fn buffer_code(b: Buffer) -> Int {
  match b { | Empty => 10 | Full(n) => n }
}
fn main() -> Int {
  let seed = @runtime { 1 + 2 };
  let s: Shape = if seed > 2 { Circle(seed) } else { Empty };
  let b: Buffer = if seed > 2 { Full(seed) } else { Empty };
  shape_code(s) + buffer_code(b)
}
"#,
        ),
        (
            // Two resume sites share one copy of the continuation, so the boxed
            // value crosses an integer dispatch on its way back into the clause.
            // ARC has to keep that edge balanced whichever arm the switch picks
            // (CIELO-39).
            "ctor_through_defunctionalised_resume",
            r#"
effect St { fn tick(n: Int) -> Int }
enum Box { B(Int) }
fn unbox(b: Box) -> Int {
  match b { B(x) => x }
}
fn main() -> Int {
  let seed = @runtime { 1 };
  handle {
    let a = do St.tick(seed);
    let b = do St.tick(0);
    unbox(B(a)) + unbox(B(b))
  } with St {
    | tick(n, resume) => if n > 0 {
        let y = resume(unbox(B(10)));
        y + 1
      } else {
        let z = resume(unbox(B(20)));
        z * 2
      }
  }
}
"#,
        ),
        (
            "ctor_through_handler",
            r#"
effect St { fn note(n: Int) -> Int }
enum Box { B(Int) }
fn main() -> Int {
  let out = handle {
    let v = do St.note(7);
    v
  } with St {
    | note(n, resume) => resume(n + 1)
  };
  match B(out) {
    B(x) => x
  }
}
"#,
        ),
    ];

    let presets = [
        ("unmanaged", MemoryPreset::Unmanaged),
        ("arc_raw", MemoryPreset::ArcRaw),
        ("arc_optimized", MemoryPreset::ArcOptimized),
    ];

    for (case_index, (name, source)) in cases.iter().enumerate() {
        let mut exits = Vec::new();
        for (preset_index, (preset_name, preset)) in presets.iter().enumerate() {
            let compiler = PassHarness::new(PassConfig::default().with_memory_preset(*preset));
            let mut interner = Interner::new();
            let source_id = SourceId::from_u32((20_000 + case_index * 10 + preset_index) as u32);
            let compiled = compiler.compile_source_to_c(source, source_id, &mut interner);
            assert!(
                !compiled.residual.diagnostics().has_errors(),
                "case {name} failed to compile under {preset_name}"
            );
            let exit = compile_and_run_c_exit_code(name, &compiled.c_source);
            if *preset != MemoryPreset::Unmanaged {
                assert_arc_balanced(&format!("{name}_{preset_name}"), &compiled.c_source);
            }
            exits.push((*preset_name, exit));
        }

        let (_, first_exit) = exits[0];
        assert!(
            exits.iter().all(|(_, exit)| *exit == first_exit),
            "memory presets disagree on case {name}: {exits:?}"
        );
    }
}

#[test]
fn runtime_exit_matches_evaluator_oracle_for_seed_expanding_handler_cases() {
    if !c_compiler_available() {
        eprintln!("skipping randomized runtime diff test: no C compiler found");
        return;
    }

    const BASE_SEEDS: [u64; 6] = [
        0xA12F_0089_8877_55C1,
        0x6B4E_13DD_F031_9223,
        0xFF00_F0F0_1234_5678,
        0x0123_4567_89AB_CDEF,
        0x0D15_EA5E_CAFE_BEEF,
        0x3141_5926_5358_9793,
    ];
    const VARIANTS_PER_SEED: usize = 5;
    const VARIANT_MIX: u64 = 0x9E37_79B9_7F4A_7C15;
    const SOURCE_ID_BASE: u32 = 10_000;

    let compiler = PassHarness::new(PassConfig::default());
    let mut case_index = 0u32;

    for base_seed in BASE_SEEDS {
        for variant in 0..VARIANTS_PER_SEED {
            let derived_seed = base_seed
                ^ ((variant as u64 + 1).wrapping_mul(VARIANT_MIX))
                ^ ((case_index as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9));
            let generated = build_generated_handler_case(derived_seed, case_index as usize);
            let mut interner = Interner::new();
            let compiled = compiler.compile_source_to_c(
                generated.source.as_str(),
                SourceId::from_u32(SOURCE_ID_BASE + case_index),
                &mut interner,
            );
            let expected = evaluator_oracle_exit_code(&compiled, &interner).unwrap_or_else(|| {
                panic!(
                    "failed to compute evaluator oracle for generated case {} (seed {:#x}, template {})\nsource:\n{}",
                    generated.name,
                    generated.seed,
                    generated.template,
                    generated.source
                )
            });
            let actual =
                compile_and_run_c_exit_code(generated.name.as_str(), compiled.c_source.as_str());
            assert_eq!(
                actual, expected,
                "runtime/evaluator mismatch for generated case {} (seed {:#x}, template {})",
                generated.name, generated.seed, generated.template
            );
            case_index += 1;
        }
    }
}

/// Nested expression shapes rather than handler shapes: operator nesting,
/// comparison and boolean chains, `if` in value position, and a `let` chain
/// feeding later expressions.
///
/// This covers the class the oracle can judge — Core evaluation disagreeing
/// with the compiled C. A rule both sides implement identically wrongly is
/// invisible here by construction; those need a stated expected value instead.
#[test]
fn runtime_exit_matches_evaluator_oracle_for_generated_expressions() {
    if !c_compiler_available() {
        eprintln!("skipping generated expression diff test: no C compiler found");
        return;
    }

    const BASE_SEEDS: [u64; 6] = [
        0x51E7_2C10_9AB3_44D2,
        0x2F80_1DDE_6C41_7735,
        0xB33F_0AC5_1928_3746,
        0x77AA_5599_CC33_EE11,
        0x1B2D_3F4A_5C6E_7081,
        0xE1D2_C3B4_A596_8778,
    ];
    const VARIANTS_PER_SEED: usize = 6;
    const VARIANT_MIX: u64 = 0x9E37_79B9_7F4A_7C15;
    const SOURCE_ID_BASE: u32 = 30_000;

    let compiler = PassHarness::new(PassConfig::default());
    let mut case_index = 0u32;

    for base_seed in BASE_SEEDS {
        for variant in 0..VARIANTS_PER_SEED {
            let derived_seed = base_seed
                ^ ((variant as u64 + 1).wrapping_mul(VARIANT_MIX))
                ^ ((case_index as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9));
            let generated = build_generated_expr_case(derived_seed, case_index as usize);
            let mut interner = Interner::new();
            let compiled = compiler.compile_source_to_c(
                generated.source.as_str(),
                SourceId::from_u32(SOURCE_ID_BASE + case_index),
                &mut interner,
            );
            assert!(
                !compiled.residual.diagnostics().has_errors(),
                "generated case {} (seed {:#x}) did not compile:\n{}\ndiagnostics: {:?}",
                generated.name,
                generated.seed,
                generated.source,
                compiled
                    .residual
                    .diagnostics()
                    .entries()
                    .iter()
                    .map(|d| d.code.to_owned())
                    .collect::<Vec<_>>()
            );
            let expected = evaluator_oracle_exit_code(&compiled, &interner).unwrap_or_else(|| {
                panic!(
                    "failed to compute evaluator oracle for generated case {} (seed {:#x})\nsource:\n{}",
                    generated.name, generated.seed, generated.source
                )
            });
            let actual =
                compile_and_run_c_exit_code(generated.name.as_str(), compiled.c_source.as_str());
            assert_eq!(
                actual, expected,
                "runtime/evaluator mismatch for generated case {} (seed {:#x})\nsource:\n{}",
                generated.name, generated.seed, generated.source
            );
            case_index += 1;
        }
    }
}

#[test]
fn scoped_perform_does_not_dispatch_to_wrong_capability() {
    if !c_compiler_available() {
        eprintln!("skipping scoped capability runtime test: no C compiler found");
        return;
    }

    let runtime_header = format!(
        "{}/../cielo-backend-c/src/cielo_runtime.h",
        env!("CARGO_MANIFEST_DIR")
    )
    .replace('\\', "\\\\");
    let c_source = format!(
        r#"
#define CIELO_RUNTIME_IMPL
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

/// Performing on a capability whose frame has been popped is a program error.
/// Falling through to another frame with the same effect label would run the
/// wrong clause, and returning unit is indistinguishable from a real result.
#[test]
fn scoped_perform_traps_when_its_capability_left_scope() {
    if !c_compiler_available() {
        eprintln!("skipping stale capability runtime test: no C compiler found");
        return;
    }

    let runtime_header = format!(
        "{}/../cielo-backend-c/src/cielo_runtime.h",
        env!("CARGO_MANIFEST_DIR")
    )
    .replace('\\', "\\\\");
    let c_source = format!(
        r#"
#define CIELO_RUNTIME_IMPL
#include <stdint.h>
#include "{runtime_header}"

static int outer_hits = 0;

static CieloValue outer_clause(CieloEvidence* evidence, CieloContinuation* continuation, size_t argc, const CieloValue* args) {{
    (void)evidence;
    (void)continuation;
    (void)argc;
    (void)args;
    outer_hits += 1;
    return cv_unit();
}}

int main(void) {{
    CieloClauseEntry outer_entries[1] = {{ {{123u, outer_clause}} }};
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
    CieloEvidence inner = outer;

    uint32_t outer_cap = cielo_handler_push_with_evidence(7u, &outer);
    uint32_t inner_cap = cielo_handler_push_with_evidence(7u, &inner);
    if (outer_cap == 0u || inner_cap == 0u) {{
        return 3;
    }}
    cielo_handler_pop(inner_cap);

    (void)cielo_perform_scoped(7u, inner_cap, 123u, "tick", 0u, NULL);
    return outer_hits;
}}
"#
    );

    let (code, stderr, _) = compile_and_run_c("stale_scoped_capability", c_source.as_str());
    assert_eq!(
        code, None,
        "a stale scoped capability must abort, not return a value"
    );
    assert!(
        stderr.contains("scoped handler is no longer in scope"),
        "expected the stale-capability trap, got: {stderr}"
    );
}

#[test]
fn callback_scoped_capability_avoids_wrong_handler_interception() {
    if !c_compiler_available() {
        eprintln!("skipping callback capability runtime test: no C compiler found");
        return;
    }

    let runtime_header = format!(
        "{}/../cielo-backend-c/src/cielo_runtime.h",
        env!("CARGO_MANIFEST_DIR")
    )
    .replace('\\', "\\\\");
    let c_source = format!(
        r#"
#define CIELO_RUNTIME_IMPL
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

#[test]
fn arc_runtime_release_frees_nested_ctor_graph() {
    if !c_compiler_available() {
        eprintln!("skipping ARC runtime graph release test: no C compiler found");
        return;
    }

    let runtime_header = format!(
        "{}/../cielo-backend-c/src/cielo_runtime.h",
        env!("CARGO_MANIFEST_DIR")
    )
    .replace('\\', "\\\\");
    let c_source = format!(
        r#"
#define CIELO_RUNTIME_IMPL
#include <stdint.h>
#include "{runtime_header}"

int main(void) {{
    cielo_arc_stats_reset();
    CieloValue left = cielo_make_ctor("Leaf", "L", 1u, 0u, NULL);
    CieloValue right = cielo_make_ctor("Leaf", "R", 2u, 0u, NULL);
    CieloValue fields[2] = {{left, right}};
    CieloValue pair = cielo_make_ctor("Pair", "Mk", 3u, 2u, fields);

    CieloArcStats after_alloc = cielo_arc_stats_snapshot();
    if (after_alloc.ctor_allocations != 3u || after_alloc.ctor_frees != 0u) {{
        return 2;
    }}

    /* Constructor fields are sink arguments: left and right moved into pair. */
    cielo_arc_release(pair);
    CieloArcStats after_root_release = cielo_arc_stats_snapshot();
    return (after_root_release.ctor_allocations == 3u &&
            after_root_release.ctor_frees == 3u &&
            after_root_release.release_last_calls == 3u)
               ? 0
               : 1;
}}
"#
    );

    let actual = compile_and_run_c_exit_code("arc_release_nested_graph", c_source.as_str());
    assert_eq!(
        actual, 0,
        "ARC runtime should release and destroy a nested constructor graph exactly once"
    );
}

#[test]
fn arc_runtime_dec_is_last_and_immortal_ctor_are_safe() {
    if !c_compiler_available() {
        eprintln!("skipping ARC runtime dec_is_last test: no C compiler found");
        return;
    }

    let runtime_header = format!(
        "{}/../cielo-backend-c/src/cielo_runtime.h",
        env!("CARGO_MANIFEST_DIR")
    )
    .replace('\\', "\\\\");
    let c_source = format!(
        r#"
#define CIELO_RUNTIME_IMPL
#include <stdint.h>
#include "{runtime_header}"

int main(void) {{
    cielo_arc_stats_reset();
    CieloValue value = cielo_make_ctor("Leaf", "One", 1u, 0u, NULL);
    CieloValue alias = value;
    cielo_arc_retain(alias);

    if (cielo_arc_dec_is_last(value)) {{
        return 2;
    }}
    if (!cielo_arc_dec_is_last(alias)) {{
        return 3;
    }}
    cielo_arc_destroy_and_dispose(alias);

    CieloArcStats after_owned = cielo_arc_stats_snapshot();
    if (after_owned.ctor_allocations != 1u || after_owned.ctor_frees != 1u) {{
        return 4;
    }}

    static CieloCtor immortal = {{
        .arc = CIELO_ARC_IMMORTAL_HEADER,
        .ty = "Immortal",
        .variant = "Root",
        .variant_tag = 9u,
        .argc = 0u,
        .fields = NULL
    }};
    CieloValue immortal_value = {{.tag = CV_CTOR, .as.ctor = &immortal}};
    cielo_arc_retain(immortal_value);
    cielo_arc_release(immortal_value);

    CieloArcStats after_immortal = cielo_arc_stats_snapshot();
    return (after_immortal.ctor_allocations == 1u &&
            after_immortal.ctor_frees == 1u)
               ? 0
               : 1;
}}
"#
    );

    let actual = compile_and_run_c_exit_code("arc_dec_is_last_immortal", c_source.as_str());
    assert_eq!(
        actual, 0,
        "ARC runtime should support dec_is_last/destroy and leave immortal ctors untouched"
    );
}

fn evaluator_oracle_exit_code(compiled: &CompiledC, interner: &Interner) -> Option<i32> {
    let program = compiled.residual.program();
    let ct = compiled.residual.ct();
    let sema = compiled.residual.sema();
    let main = find_main_body(program, interner)?;

    let mut env = HashMap::new();
    let mut handler_stack = Vec::new();
    let mut continuations = Vec::new();
    let value = eval_stmt(
        program,
        ct,
        sema,
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
    sema: &SemanticTables,
    stmt_id: StmtId,
    env: &mut HashMap<VarId, OracleValue>,
    handler_stack: &mut Vec<HandlerFrame>,
    continuations: &mut Vec<Continuation>,
) -> Option<OracleValue> {
    eval_stmt_at(
        program,
        ct,
        sema,
        stmt_id,
        env,
        handler_stack,
        continuations,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn eval_stmt_at(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    sema: &SemanticTables,
    stmt_id: StmtId,
    env: &mut HashMap<VarId, OracleValue>,
    handler_stack: &mut Vec<HandlerFrame>,
    continuations: &mut Vec<Continuation>,
    depth: usize,
) -> Option<OracleValue> {
    if depth > ORACLE_MAX_CALL_DEPTH {
        return None;
    }
    let stmt = program.stmt(stmt_id)?;
    match &stmt.kind {
        StmtKind::Return(expr) => eval_expr(program, ct, sema, *expr, env),
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            let value = eval_expr(program, ct, sema, *value, env)?;
            env.insert(*binding, value);
            eval_stmt(program, ct, sema, *next, env, handler_stack, continuations)
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
                sema,
                *value,
                &mut value_env,
                handler_stack,
                continuations,
            )?;
            env.insert(*binding, value);
            eval_stmt(program, ct, sema, *next, env, handler_stack, continuations)
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => match eval_expr(program, ct, sema, *cond, env)? {
            OracleValue::Bool(true) => {
                let mut then_env = env.clone();
                eval_stmt(
                    program,
                    ct,
                    sema,
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
                    sema,
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
            sema,
            PerformSite {
                effect: *effect,
                operation: *operation,
                args,
                result: *result,
                next: *next,
            },
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
            let arg_value = eval_expr(program, ct, sema, *arg, env)?;
            let resume_id = match env.get(resume).cloned()? {
                OracleValue::ResumeToken(id) => id,
                _ => return None,
            };
            let resumed =
                resume_continuation(program, ct, sema, continuations, resume_id, arg_value)?;
            env.insert(*result, resumed);
            eval_stmt(program, ct, sema, *next, env, handler_stack, continuations)
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
            let body_value =
                eval_stmt(program, ct, sema, *body, env, handler_stack, continuations)?;
            handler_stack.pop();

            let handler_def = program.handlers().get(handler.index())?;
            let mut return_env = env.clone();
            return_env.insert(handler_def.return_param, body_value);
            let handled_value = eval_stmt(
                program,
                ct,
                sema,
                handler_def.return_body,
                &mut return_env,
                handler_stack,
                continuations,
            )?;
            if let Some(next_stmt) = next {
                eval_stmt(
                    program,
                    ct,
                    sema,
                    *next_stmt,
                    env,
                    handler_stack,
                    continuations,
                )
            } else {
                Some(handled_value)
            }
        }
        StmtKind::Stage { body, next, .. } => {
            let body_value =
                eval_stmt(program, ct, sema, *body, env, handler_stack, continuations)?;
            if let Some(next_stmt) = next {
                eval_stmt(
                    program,
                    ct,
                    sema,
                    *next_stmt,
                    env,
                    handler_stack,
                    continuations,
                )
            } else {
                Some(body_value)
            }
        }
        StmtKind::Call {
            result,
            callee,
            args,
            next,
            ..
        } => {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(eval_expr_at(program, ct, sema, *arg, env, depth)?);
            }
            let returned = eval_call(
                program,
                ct,
                sema,
                *callee,
                values,
                handler_stack,
                continuations,
                depth + 1,
            )?;
            env.insert(*result, returned);
            eval_stmt_at(
                program,
                ct,
                sema,
                *next,
                env,
                handler_stack,
                continuations,
                depth,
            )
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            let value = eval_expr_at(program, ct, sema, *scrutinee, env, depth)?;
            let OracleValue::Ctor { variant, fields } = value else {
                return None;
            };
            let arm = arms.iter().find(|arm| arm.tag == variant);
            let body = match arm {
                Some(arm) => {
                    if arm.binders.len() != fields.len() {
                        return None;
                    }
                    for (binder, field) in arm.binders.iter().zip(fields) {
                        env.insert(*binder, field);
                    }
                    arm.body
                }
                None => (*default)?,
            };
            eval_stmt_at(
                program,
                ct,
                sema,
                body,
                env,
                handler_stack,
                continuations,
                depth,
            )
        }
        StmtKind::Hole { .. } | StmtKind::Error(_) => None,
    }
}

struct PerformSite<'a> {
    effect: cielo_base::EffectLabelId,
    operation: SymbolId,
    args: &'a [ExprId],
    result: Option<VarId>,
    next: StmtId,
}

fn eval_perform(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    sema: &SemanticTables,
    site: PerformSite<'_>,
    env: &mut HashMap<VarId, OracleValue>,
    handler_stack: &mut [HandlerFrame],
    continuations: &mut Vec<Continuation>,
) -> Option<OracleValue> {
    let mut arg_values = Vec::with_capacity(site.args.len());
    for arg in site.args {
        arg_values.push(eval_expr(program, ct, sema, *arg, env)?);
    }

    let mut selected = None;
    for (idx, frame) in handler_stack.iter().enumerate().rev() {
        let handler_def = program.handlers().get(frame.handler.index())?;
        if handler_def.effect != site.effect {
            continue;
        }
        if let Some(clause_idx) = handler_def
            .clauses
            .iter()
            .position(|clause| clause.operation == site.operation)
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
        handler_stack: handler_stack.to_vec(),
        next: site.next,
        result: site.result,
        used: false,
    });

    let mut clause_env = frame.captured_env;
    for (param, value) in clause.params.iter().zip(arg_values.iter().cloned()) {
        clause_env.insert(*param, value);
    }
    if let Some(resume_param) = clause.resume_param {
        clause_env.insert(resume_param, OracleValue::ResumeToken(continuation_id));
    }

    let mut clause_stack = handler_stack[..frame_index].to_vec();
    eval_stmt(
        program,
        ct,
        sema,
        clause.body,
        &mut clause_env,
        &mut clause_stack,
        continuations,
    )
}

fn resume_continuation(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    sema: &SemanticTables,
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
        sema,
        next,
        &mut env,
        &mut handler_stack,
        continuations,
    )
}

fn eval_expr(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    sema: &SemanticTables,
    expr_id: ExprId,
    env: &HashMap<VarId, OracleValue>,
) -> Option<OracleValue> {
    eval_expr_at(program, ct, sema, expr_id, env, 0)
}

fn eval_expr_at(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    sema: &SemanticTables,
    expr_id: ExprId,
    env: &HashMap<VarId, OracleValue>,
    depth: usize,
) -> Option<OracleValue> {
    if depth > ORACLE_MAX_CALL_DEPTH {
        return None;
    }
    let expr = program.expr(expr_id)?;
    match &expr.kind {
        ExprKind::Literal(literal) => literal_to_oracle(literal),
        ExprKind::Var(var) => env.get(var).cloned(),
        ExprKind::Unary { op, expr } => {
            let value = eval_expr_at(program, ct, sema, *expr, env, depth)?;
            eval_unary(*op, value)
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let left = eval_expr_at(program, ct, sema, *lhs, env, depth)?;
            let right = eval_expr_at(program, ct, sema, *rhs, env, depth)?;
            eval_binary(*op, left, right)
        }
        ExprKind::MakeStruct { fields, .. } => {
            let mut values = Vec::with_capacity(fields.len());
            for field in fields {
                values.push(eval_expr_at(program, ct, sema, *field, env, depth)?);
            }
            Some(OracleValue::Ctor {
                variant: SymbolId::INVALID,
                fields: values,
            })
        }
        ExprKind::MakeEnum {
            variant, fields, ..
        } => {
            let mut values = Vec::with_capacity(fields.len());
            for field in fields {
                values.push(eval_expr_at(program, ct, sema, *field, env, depth)?);
            }
            Some(OracleValue::Ctor {
                variant: *variant,
                fields: values,
            })
        }
        ExprKind::Field { base, .. } => {
            let index = *sema.field_index_of_expr.get(&expr_id)?;
            match eval_expr_at(program, ct, sema, *base, env, depth)? {
                OracleValue::Ctor { fields, .. } => fields.get(index as usize).cloned(),
                _ => None,
            }
        }
        ExprKind::PureCall { callee, args } => {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(eval_expr_at(program, ct, sema, *arg, env, depth)?);
            }
            // A `PureCall` has an empty effect row, so it needs no frames.
            eval_call(
                program,
                ct,
                sema,
                *callee,
                values,
                &mut Vec::new(),
                &mut Vec::new(),
                depth + 1,
            )
        }
        // Builtins produce output rather than a value the oracle can model.
        ExprKind::BuiltinCall { .. } | ExprKind::Error(_) => None,
    }
}

/// Handlers are deep: the caller's frames stay on the stack across a call, so a
/// callee performing an effect its caller handles resolves against them. A
/// fresh stack here instead made every such program unevaluatable, which the
/// seeded cases could only report as "no oracle".
#[allow(clippy::too_many_arguments)]
fn eval_call(
    program: &CoreProgram,
    ct: &CtPropagationTables,
    sema: &SemanticTables,
    callee: cielo_base::FuncId,
    args: Vec<OracleValue>,
    handler_stack: &mut Vec<HandlerFrame>,
    continuations: &mut Vec<Continuation>,
    depth: usize,
) -> Option<OracleValue> {
    if depth > ORACLE_MAX_CALL_DEPTH {
        return None;
    }
    let function = program.function(callee)?;
    if function.params.len() != args.len() {
        return None;
    }
    let mut env = HashMap::new();
    for (param, value) in function.params.iter().zip(args) {
        env.insert(*param, value);
    }
    eval_stmt_at(
        program,
        ct,
        sema,
        function.body,
        &mut env,
        handler_stack,
        continuations,
        depth,
    )
}

fn eval_unary(op: UnaryOp, value: OracleValue) -> Option<OracleValue> {
    match (op, value) {
        (UnaryOp::Neg, OracleValue::Int(value)) => Some(OracleValue::Int(value.checked_neg()?)),
        (UnaryOp::Not, OracleValue::Bool(value)) => Some(OracleValue::Bool(!value)),
        _ => None,
    }
}

/// `None` means "no oracle for this case" and the harness skips it, which is
/// the only honest answer for arithmetic the C runtime traps on: it dies by
/// signal rather than producing a value to compare against. Wrapping here would
/// assert a result the compiled program never returns. `i64::MIN % -1` is the
/// one exception -- cv_mod defines it as 0 instead of trapping.
fn eval_binary(op: BinaryOp, lhs: OracleValue, rhs: OracleValue) -> Option<OracleValue> {
    match (op, lhs, rhs) {
        (BinaryOp::Add, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Int(lhs.checked_add(rhs)?))
        }
        (BinaryOp::Sub, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Int(lhs.checked_sub(rhs)?))
        }
        (BinaryOp::Mul, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Int(lhs.checked_mul(rhs)?))
        }
        (BinaryOp::Div, OracleValue::Int(lhs), OracleValue::Int(rhs)) => {
            Some(OracleValue::Int(lhs.checked_div(rhs)?))
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
        OracleValue::ResumeToken(_) | OracleValue::Ctor { .. } => return None,
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

/// Runs the program and fails if any constructor or string outlives it. The
/// emitted `main` is renamed so a wrapper can read the ARC counters after it
/// returns. Strings are counted separately from constructors, so each total
/// has to balance on its own.
fn assert_arc_balanced(case_name: &str, c_source: &str) {
    const WRAPPER: &str = concat!(
        "\nint cielo_checked_entry(void);\n",
        "int main(void) {\n",
        "  int code = cielo_checked_entry();\n",
        "  CieloArcStats stats = cielo_arc_stats_snapshot();\n",
        "  if (stats.ctor_allocations != stats.ctor_frees) {\n",
        "    fprintf(stderr, \"arc imbalance: %llu allocations, %llu frees\\n\",\n",
        "            (unsigned long long)stats.ctor_allocations,\n",
        "            (unsigned long long)stats.ctor_frees);\n",
        "    return 90;\n",
        "  }\n",
        "  if (stats.str_allocations != stats.str_frees) {\n",
        "    fprintf(stderr, \"string imbalance: %llu allocations, %llu frees\\n\",\n",
        "            (unsigned long long)stats.str_allocations,\n",
        "            (unsigned long long)stats.str_frees);\n",
        "    return 91;\n",
        "  }\n",
        "  return code;\n",
        "}\n",
    );
    let renamed = c_source.replace("int main(void) {", "int cielo_checked_entry(void) {");
    assert!(
        renamed != c_source,
        "case {case_name}: emitted C has no `int main(void)` to rename"
    );
    let instrumented = format!("{renamed}{WRAPPER}");
    let code = compile_and_run_c_exit_code(&format!("{case_name}_arcbalance"), &instrumented);
    assert_ne!(
        code, 90,
        "case {case_name} leaked constructors: allocations != frees"
    );
    assert_ne!(
        code, 91,
        "case {case_name} leaked strings: allocations != frees"
    );
}

/// Exit code (`None` when a signal killed it), stderr, stdout.
fn compile_and_run_c(case_name: &str, c_source: &str) -> (Option<i32>, String, String) {
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
        .arg("-DCIELO_ARC_STATS")
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
    let outcome = (
        run.status.code(),
        String::from_utf8_lossy(run.stderr.as_slice()).into_owned(),
        String::from_utf8_lossy(run.stdout.as_slice()).into_owned(),
    );
    let _ = fs::remove_dir_all(work_dir.as_path());
    outcome
}

fn compile_and_run_c_exit_code(case_name: &str, c_source: &str) -> i32 {
    let (code, stderr, stdout) = compile_and_run_c(case_name, c_source);
    code.unwrap_or_else(|| {
        panic!(
            "runtime execution terminated by signal for case {case_name}:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )
    })
}
