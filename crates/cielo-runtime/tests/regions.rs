use cielo_base::{EffectLabelId, LinearFuncId, SymbolId, VarId};
use cielo_ir::cfg::{CfgInstruction, CfgProgram};
use cielo_ir::core::Literal;
use cielo_ir::linear::{LinearExpr, LinearFunction, LinearProgram, LinearStmt};
use cielo_ir::region::{Placement, RegionOwner, RegionSlotKind};
use cielo_runtime::cfg_lower;
use cielo_runtime::region::RegionPlacementStats;

const HANDLED: EffectLabelId = EffectLabelId::INVALID;

fn lower(linear: &LinearProgram) -> CfgProgram {
    let cfg = cfg_lower::run(linear);
    cfg.validate().expect("lowered CFG should be valid");
    cfg
}

fn single_function(linear: &mut LinearProgram, body: cielo_base::LinearStmtId) {
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(0),
        params: vec![],
        body,
    });
    linear.entrypoints.push(LinearFuncId::from_u32(0));
}

fn only_placement(cfg: &CfgProgram) -> Placement {
    let regions = cfg.regions();
    assert_eq!(regions.len(), 1, "fixture should open exactly one region");
    let slots = &regions[0].slots;
    assert_eq!(slots.len(), 1, "a handler region holds one evidence slot");
    let RegionSlotKind::HandlerEvidence { .. } = slots[0].kind;
    slots[0].placement
}

/// A handler body that only computes cannot leak the evidence address.
fn confined_handler() -> LinearProgram {
    let mut linear = LinearProgram::default();
    let zero = linear.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let body = linear.push_stmt(LinearStmt::Return(zero));
    let handle = linear.push_stmt(LinearStmt::Handle {
        effect: HANDLED,
        body,
        next: None,
    });
    single_function(&mut linear, handle);
    linear
}

#[test]
fn a_handle_brackets_its_body_with_region_operations() {
    let cfg = lower(&confined_handler());
    let region = cfg.regions().first().expect("handle opens a region");
    let RegionOwner::Handler { handler: owner, .. } = region.owner;

    let mut enters = 0;
    let mut exits = 0;
    for block in cfg.blocks() {
        let mut seen: Vec<&CfgInstruction> = Vec::new();
        for instruction in &block.instructions {
            seen.push(&cfg.instruction(*instruction).expect("known").kind);
        }
        for (index, kind) in seen.iter().enumerate() {
            match kind {
                CfgInstruction::RegionEnter { region: found } => {
                    assert_eq!(*found, region.id);
                    enters += 1;
                    // Storage has to exist before its address reaches the
                    // handler stack, so the open cannot follow the push.
                    assert!(matches!(
                        seen.get(index + 1),
                        Some(CfgInstruction::HandlerEnter { handler, .. }) if *handler == owner
                    ));
                }
                CfgInstruction::RegionExit { region: found } => {
                    assert_eq!(*found, region.id);
                    exits += 1;
                    assert!(matches!(
                        index.checked_sub(1).and_then(|prev| seen.get(prev)),
                        Some(CfgInstruction::HandlerExit { handler, .. }) if *handler == owner
                    ));
                }
                _ => {}
            }
        }
    }
    assert_eq!(enters, 1, "one open per handler installation");
    assert_eq!(exits, 1, "one close per handler installation");
}

#[test]
fn a_confined_handler_region_is_stack_placed() {
    let cfg = lower(&confined_handler());
    assert_eq!(only_placement(&cfg), Placement::Stack);
    assert_eq!(
        RegionPlacementStats::of(&cfg),
        RegionPlacementStats {
            stack_slots: 1,
            arena_slots: 0
        }
    );
}

#[test]
fn performing_the_handled_effect_stays_confined() {
    let mut linear = LinearProgram::default();
    let zero = linear.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let tail = linear.push_stmt(LinearStmt::Return(zero));
    let perform = linear.push_stmt(LinearStmt::Perform {
        result: None,
        effect: HANDLED,
        operation: SymbolId::from_u32(1),
        args: vec![],
        next: tail,
    });
    let handle = linear.push_stmt(LinearStmt::Handle {
        effect: HANDLED,
        body: perform,
        next: None,
    });
    single_function(&mut linear, handle);

    let cfg = lower(&linear);
    assert_eq!(only_placement(&cfg), Placement::Stack);
}

/// An effect this region does not handle unwinds past it, and nothing here
/// bounds when the outer handler resumes relative to the close.
#[test]
fn performing_an_unhandled_effect_forces_the_arena() {
    let mut linear = LinearProgram::default();
    let zero = linear.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let tail = linear.push_stmt(LinearStmt::Return(zero));
    let perform = linear.push_stmt(LinearStmt::Perform {
        result: None,
        effect: EffectLabelId::from_u32(7),
        operation: SymbolId::from_u32(1),
        args: vec![],
        next: tail,
    });
    let handle = linear.push_stmt(LinearStmt::Handle {
        effect: HANDLED,
        body: perform,
        next: None,
    });
    single_function(&mut linear, handle);

    let cfg = lower(&linear);
    assert_eq!(only_placement(&cfg), Placement::Arena);
}

/// A control call may suspend, so the evidence may outlive the C frame.
#[test]
fn a_control_call_inside_the_region_forces_the_arena() {
    let mut linear = LinearProgram::default();
    let zero = linear.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let callee_body = linear.push_stmt(LinearStmt::Return(zero));
    let result = VarId::from_u32(0);
    let value = linear.push_expr(LinearExpr::Var(result));
    let tail = linear.push_stmt(LinearStmt::Return(value));
    let call = linear.push_stmt(LinearStmt::ControlCall {
        result,
        callee: SymbolId::from_u32(2),
        callee_fn: LinearFuncId::from_u32(1),
        args: vec![],
        next: tail,
    });
    let handle = linear.push_stmt(LinearStmt::Handle {
        effect: HANDLED,
        body: call,
        next: None,
    });
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(0),
        params: vec![],
        body: handle,
    });
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(1),
        name: SymbolId::from_u32(2),
        params: vec![],
        body: callee_body,
    });
    linear.entrypoints.push(LinearFuncId::from_u32(0));

    let cfg = lower(&linear);
    assert_eq!(only_placement(&cfg), Placement::Arena);
}

/// A CFG that never reached `place` must read as arena-backed, because that is
/// the answer that still frees correctly.
#[test]
fn an_unplaced_region_defaults_to_the_arena() {
    let linear = confined_handler();
    let cfg = cfg_lower::lower_program(&linear);
    assert_eq!(only_placement(&cfg), Placement::Arena);
}
