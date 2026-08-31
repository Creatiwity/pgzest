//! Types de diagnostic partagés par toute la chaîne d'outils.
//!
//! Ce crate est volontairement minimal et sans dépendance interne : c'est le
//! socle commun du checker, du linter, du codegen, du resolver de schéma et
//! des plugins. Il vivait auparavant dans `pgsql-checker`, ce qui forçait
//! chaque consommateur de `Diagnostic` à compiler le type-checker entier.

pub mod codes;
pub mod diagnostic;

pub use codes::*;
pub use diagnostic::{Diagnostic, Severity, Span};

/// Applique les surcharges de sévérité par règle à une liste de diagnostics.
/// Les règles à "off" sont retirées ; les autres voient leur sévérité mise à jour.
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
