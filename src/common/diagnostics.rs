use crate::common::ids::DiagnosticId;
use crate::common::span::Span;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub id: DiagnosticId,
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ErrorNode {
    pub span: Span,
    pub message: String,
    pub diagnostic: DiagnosticId,
}

#[derive(Clone, Debug, Default)]
pub struct DiagnosticBag {
    entries: Vec<Diagnostic>,
}

impl DiagnosticBag {
    pub fn entries(&self) -> &[Diagnostic] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<Diagnostic> {
        self.entries
    }

    pub fn push(
        &mut self,
        severity: Severity,
        code: &'static str,
        message: impl Into<String>,
        span: Span,
    ) -> DiagnosticId {
        let id = DiagnosticId::new(self.entries.len());
        self.entries.push(Diagnostic {
            id,
            severity,
            code,
            message: message.into(),
            span,
        });
        id
    }

    pub fn error(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
        span: Span,
    ) -> DiagnosticId {
        self.push(Severity::Error, code, message, span)
    }

    pub fn warning(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
        span: Span,
