//! Diagnostic types shared by the whole toolchain.
//!
//! This crate is deliberately minimal and has no internal dependency: it is
//! the common foundation of the checker, the linter, codegen, the schema
//! resolver and plugins. It used to live in `pgweave-checker`, which forced
//! every consumer of `Diagnostic` to compile the whole type-checker.

pub mod codes;
pub mod diagnostic;
pub mod directive;

pub use codes::*;
pub use diagnostic::{Diagnostic, Severity, Span};
pub use directive::{Directive, DirectiveKind, Suppressions};

/// Applies per-rule severity overrides to a list of diagnostics.
/// Rules set to "off" are removed; the others get their severity updated.
pub fn apply_rule_overrides(
    diags: &mut Vec<Diagnostic>,
    rules: &std::collections::HashMap<String, String>,
) {
    if rules.is_empty() {
        return;
    }
    diags.retain_mut(|d| {
        if let Some(level) = rules.get(d.code) {
            match level.as_str() {
                "off" => return false,
                "error" => d.severity = Severity::Error,
                "warning" => d.severity = Severity::Warning,
                "info" => d.severity = Severity::Info,
                _ => {}
            }
        }
        true
    });
}
