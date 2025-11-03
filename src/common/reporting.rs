use miette::{GraphicalReportHandler, LabeledSpan, MietteDiagnostic, NamedSource, Report};

use crate::common::diagnostics::{Diagnostic, Severity};

pub fn diagnostic_report(diag: &Diagnostic, source_name: &str, source: &str) -> Report {
    let source_len = source.len();
    let start = (diag.span.start as usize).min(source_len);
    let end = (diag.span.end as usize).min(source_len);
    let span_len = end.saturating_sub(start);
    let label = if span_len == 0 {
        LabeledSpan::at_offset(start, diag.message.clone())
    } else {
        LabeledSpan::new_primary_with_span(Some(diag.message.clone()), (start, span_len))
    };
    let severity = match diag.severity {
        Severity::Error => miette::Severity::Error,
        Severity::Warning => miette::Severity::Warning,
        Severity::Note => miette::Severity::Advice,
    };

    let diagnostic = MietteDiagnostic::new(diag.message.clone())
        .with_code(format!("cielo::{}", diag.code))
        .with_severity(severity)
        .with_label(label);
    Report::new(diagnostic).with_source_code(NamedSource::new(source_name, source.to_owned()))
}

pub fn render_diagnostic(diag: &Diagnostic, source_name: &str, source: &str) -> String {
    let report = diagnostic_report(diag, source_name, source);
    let handler = GraphicalReportHandler::new()
        .with_width(120)
        .with_context_lines(1)
        .without_cause_chain();
    let mut rendered = String::new();
    let _ = handler.render_report(&mut rendered, &*report);
    rendered
}
