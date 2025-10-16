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
    ) -> DiagnosticId {
        self.push(Severity::Warning, code, message, span)
    }

    pub fn note(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
        span: Span,
    ) -> DiagnosticId {
        self.push(Severity::Note, code, message, span)
    }

    pub fn error_node(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
        span: Span,
    ) -> ErrorNode {
        let message = message.into();
        let id = self.error(code, message.clone(), span);
        ErrorNode {
            span,
            message,
            diagnostic: id,
        }
    }

    pub fn has_errors(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.severity == Severity::Error)
    }

    pub fn extend(&mut self, other: DiagnosticBag) {
        let offset = self.entries.len();
        for mut entry in other.entries {
            entry.id = DiagnosticId::new(entry.id.index() + offset);
            self.entries.push(entry);
        }
    }
}
