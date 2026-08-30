/// Compiler diagnostic severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

/// A stable, source-addressed compiler diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesignDiagnostic {
    pub severity: DiagnosticSeverity,
    pub code: &'static str,
    pub path: String,
    pub message: String,
    pub suggestion: Option<String>,
}

/// A resolved compile stage value and every non-fatal diagnostic it emitted.
#[derive(Clone, Debug, PartialEq)]
pub struct CompileSuccess<T> {
    pub value: T,
    pub diagnostics: Vec<DesignDiagnostic>,
}

#[cfg(feature = "compiler")]
impl DesignDiagnostic {
    pub(crate) fn error(
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            code,
            path: path.into(),
            message: message.into(),
            suggestion: None,
        }
    }

    pub(crate) fn warning(
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            code,
            path: path.into(),
            message: message.into(),
            suggestion: None,
        }
    }

    pub(crate) fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }
}
