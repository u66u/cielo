use cielo_ir::boundary::{ModuleFacts, RuntimeModule, StagedCore, TypedCore};
use cielo_memory::{
    MemoryInput, MemoryModel, MemoryModule, MemoryProfile, RuntimeRequirement, lower, refcount,
    regions, tracing,
};

fn runtime() -> RuntimeModule {
    RuntimeModule {
        staged: StagedCore {
            typed: TypedCore {
                module: ModuleFacts {
                    items: 4,
                    functions: 2,
                    ..ModuleFacts::default()
                },
                declarations: 4,
                callable_effects: 0,
            },
            compile_time_functions: 1,
            residual_items: 3,
        },
        allocations: 2,
        calls: 2,
        suspension_points: 1,
    }
}

#[test]
fn each_strategy_has_its_own_manifest() {
    let runtime = runtime();
    let input = MemoryInput { runtime: &runtime };

    let rc = MemoryModule::ReferenceCounting(refcount::lower(input, Default::default()));
    let tracing = MemoryModule::Tracing(tracing::lower(input, Default::default()));
    let regions = MemoryModule::Regions(regions::lower(input, Default::default()));

    assert!(
        rc.manifest()
            .requirements()
            .contains(&RuntimeRequirement::ReferenceCounting)
    );
    assert!(
        tracing
            .manifest()
            .requirements()
            .contains(&RuntimeRequirement::TracingCollector)
    );
    assert!(
        regions
            .manifest()
            .requirements()
            .contains(&RuntimeRequirement::RegionAllocator)
    );
}

#[test]
fn dispatcher_selects_only_the_requested_family() {
    let runtime = runtime();
    let input = MemoryInput { runtime: &runtime };
    let result = lower(
        input,
        MemoryProfile {
            model: MemoryModel::Regions,
            ..MemoryProfile::default()
        },
    );
    assert!(matches!(result, MemoryModule::Regions(_)));
}
