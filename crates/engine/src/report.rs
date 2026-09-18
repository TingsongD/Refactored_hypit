//! Render engine diagnostics as rustc-style source errors via miette.

use std::fmt;

use miette::{LabeledSpan, NamedSource, SourceCode};
use scene_ir::{Diagnostic, Severity};

pub struct DiagReport {
    source: NamedSource<String>,
    diag: Diagnostic,
}

impl DiagReport {
    pub fn new(source_name: &str, source: String, diag: Diagnostic) -> Self {
        Self {
            source: NamedSource::new(source_name, source),
            diag,
        }
    }
}

impl fmt::Debug for DiagReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for DiagReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.diag.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{level}: {}", self.diag.message)
    }
}

impl std::error::Error for DiagReport {}

impl miette::Diagnostic for DiagReport {
    fn source_code(&self) -> Option<&dyn SourceCode> {
        Some(&self.source)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let span = self.diag.span?;
        Some(Box::new(std::iter::once(LabeledSpan::new_with_span(
            Some(self.diag.message.clone()),
            span.start..span.end,
        ))))
    }

    fn severity(&self) -> Option<miette::Severity> {
        Some(match self.diag.severity {
            Severity::Error => miette::Severity::Error,
            Severity::Warning => miette::Severity::Warning,
        })
    }
}

/// Print all diagnostics for one source file to stderr.
pub fn emit(source_name: &str, source: &str, diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        let report = DiagReport::new(source_name, source.to_string(), diag.clone());
        eprintln!("{:?}", miette::Report::new(report));
    }
}
