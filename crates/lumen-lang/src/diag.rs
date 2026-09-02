//! Diagnostics.
//!
//! **Error messages are a product surface**, not a debugging aid for the
//! compiler author. Every diagnostic carries a span, a statement of what is
//! wrong, and a *help* line saying what to do about it — the help is required,
//! not optional, because "unexpected token" without a suggestion is where a
//! newcomer gives up.
//!
//! `examples/failing/` in `lumen-effects` asserts the specific diagnostic each
//! bad program produces, so a change that makes a message vaguer fails CI.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::ops::Range;

/// A byte range in the source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }

    /// An empty span, for diagnostics about a whole file.
    pub const EMPTY: Span = Span { start: 0, end: 0 };

    pub const fn range(self) -> Range<usize> {
        self.start..self.end
    }

    /// The smallest span covering both.
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// How much a diagnostic matters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    /// Compilation fails.
    Error,
    /// Compilation succeeds, but something is probably wrong.
    ///
    /// The warning list is deliberately long — a `let` that could not be
    /// hoisted, precision loss, a mask that gates nothing, a channel declared
    /// but never read. Each of these is a real bug an author would otherwise
    /// find by staring at lights.
    Warning,
}

/// One problem with a program.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub span: Span,
    /// What is wrong.
    pub message: String,
    /// What to do about it. Never empty.
    pub help: String,
}

impl Diagnostic {
    pub fn error(span: Span, message: impl Into<String>, help: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            span,
            message: message.into(),
            help: help.into(),
        }
    }

    pub fn warning(span: Span, message: impl Into<String>, help: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Warning,
            span,
            message: message.into(),
            help: help.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Render with the offending line and a caret, given the original source.
    ///
    /// One-based line and column, because that is what every editor shows.
    pub fn render(&self, src: &str) -> String {
        let (line_no, col, line) = locate(src, self.span.start);
        let mut out = String::new();
        let label = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let _ = writeln!(out, "{label}: {}", self.message);
        let _ = writeln!(out, "  --> line {line_no}, column {col}");
        let _ = writeln!(out, "   | {line}");
        let width = (self.span.end.saturating_sub(self.span.start)).max(1);
        let _ = writeln!(
            out,
            "   | {}{}",
            " ".repeat(col.saturating_sub(1)),
            "^".repeat(width)
        );
        let _ = write!(out, "   = help: {}", self.help);
        out
    }
}

/// Line number, column, and the text of the line containing `offset`.
fn locate(src: &str, offset: usize) -> (usize, usize, &str) {
    let offset = offset.min(src.len());
    let before = &src[..offset];
    let line_no = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line_end = src[line_start..]
        .find('\n')
        .map_or(src.len(), |i| line_start + i);
    let col = offset - line_start + 1;
    (line_no, col, &src[line_start..line_end])
}

/// A collection of diagnostics.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Diagnostics {
    pub items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Diagnostics {
        Diagnostics::default()
    }

    pub fn push(&mut self, d: Diagnostic) {
        self.items.push(d);
    }

    pub fn extend(&mut self, other: impl IntoIterator<Item = Diagnostic>) {
        self.items.extend(other);
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(Diagnostic::is_error)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter().filter(|d| d.is_error())
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter().filter(|d| !d.is_error())
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Render every diagnostic against the source.
    pub fn render(&self, src: &str) -> String {
        let mut out = String::new();
        for (i, d) in self.items.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\n");
            }
            out.push_str(&d.render(src));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_merge_to_cover_both() {
        let a = Span::new(2, 5);
        let b = Span::new(10, 12);
        assert_eq!(a.merge(b), Span::new(2, 12));
        assert_eq!(b.merge(a), Span::new(2, 12));
        assert_eq!(a.range(), 2..5);
    }

    #[test]
    fn a_diagnostic_points_at_the_right_line_and_column() {
        let src = "line one\nline two\nline three";
        let d = Diagnostic::error(Span::new(9, 13), "something", "do something else");
        let text = d.render(src);
        assert!(text.contains("line 2, column 1"), "{text}");
        assert!(text.contains("line two"), "{text}");
        assert!(text.contains("^^^^"), "{text}");
        assert!(text.contains("help: do something else"), "{text}");
    }

    #[test]
    fn a_zero_width_span_still_gets_a_caret() {
        // Spans at end-of-file are empty; a diagnostic with no visible marker
        // reads like a rendering bug.
        let src = "abc";
        let d = Diagnostic::error(Span::new(3, 3), "unexpected end", "add something");
        assert!(d.render(src).contains('^'));
    }

    #[test]
    fn an_offset_past_the_end_does_not_panic() {
        let d = Diagnostic::error(Span::new(999, 1000), "x", "y");
        let _ = d.render("short");
    }

    #[test]
    fn errors_and_warnings_are_separable() {
        let mut ds = Diagnostics::new();
        assert!(ds.is_empty());
        ds.push(Diagnostic::warning(Span::EMPTY, "w", "h"));
        assert!(!ds.has_errors());
        ds.push(Diagnostic::error(Span::EMPTY, "e", "h"));
        assert!(ds.has_errors());
        assert_eq!(ds.errors().count(), 1);
        assert_eq!(ds.warnings().count(), 1);
        assert_eq!(ds.len(), 2);
    }

    #[test]
    fn rendering_several_diagnostics_separates_them() {
        let mut ds = Diagnostics::new();
        ds.push(Diagnostic::error(Span::new(0, 1), "first", "a"));
        ds.push(Diagnostic::error(Span::new(0, 1), "second", "b"));
        let text = ds.render("x");
        assert!(text.contains("first"));
        assert!(text.contains("second"));
        assert!(text.contains("\n\n"));
    }

    #[test]
    fn every_diagnostic_carries_help() {
        // The help line is required, not optional: "unexpected token" with no
        // suggestion is where a newcomer gives up.
        let d = Diagnostic::error(Span::EMPTY, "m", "h");
        assert!(!d.help.is_empty());
    }
}
