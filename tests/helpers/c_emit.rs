#![allow(dead_code)]

pub fn assert_arc_trace_comments_align(c_source: &str) {
    let mut saw_traced_arc_op = false;
    let mut pending_trace: Option<&str> = None;

    for line in c_source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(trace) = pending_trace.take() {
            if trace == "retain" {
                assert!(
                    trimmed.starts_with("cielo_arc_retain("),
                    "retain trace comment must be followed by retain call, got `{trimmed}`"
                );
            } else {
                assert!(
                    trimmed.starts_with("cielo_arc_release("),
                    "release trace comment must be followed by release call, got `{trimmed}`"
                );
            }
            saw_traced_arc_op = true;
            continue;
        }

        if trimmed.starts_with("/* arc pre-retain s") {
            pending_trace = Some("retain");
        } else if trimmed.starts_with("/* arc post-release s") {
            pending_trace = Some("release");
        }
    }

    assert!(
        pending_trace.is_none(),
        "arc trace comment must not be left without a following runtime call"
    );
    assert!(
        saw_traced_arc_op,
        "fixture should produce traced ARC runtime calls so ordering snapshot gate is meaningful"
    );
}
