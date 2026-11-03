//! ESLint-style comment directives to switch checks off locally.
//!
//! Recognized syntax, in a line comment (`--`) or a block comment (`/* */`):
//!
//! ```sql
//! -- pgweave-disable-next-line                        -- all rules
//! -- pgweave-disable-next-line column-not-found       -- one rule
//! -- pgweave-disable-next-line column-not-found, L001 -- several
//! -- pgweave-enable-next-line L001                    -- re-enable for one line
//! -- pgweave-disable                                  -- start of block
//! -- pgweave-enable                                   -- end of block
//! -- pgweave-disable-file                             -- whole file
//! ```
//!
//! A `-- pgweave-disable` without `-- pgweave-enable` extends to the end of the file.
//!
//! A reason may be appended after `--`; the directive parser ignores it, which
//! lets the suppression be documented:
//! `-- pgweave-disable-next-line column-not-found -- column created by migration 42`
//!
//! ## Limitation
//!
//! Suppression is positional: it needs the diagnostic's `span`.
//! A diagnostic without a span (some rules do not fill it yet) can only be
//! suppressed by `pgweave-disable-file` or by a block covering the whole
//! file. See [`Suppressions::is_suppressed`].

use std::collections::HashMap;

use crate::Diagnostic;

// ── Model ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveKind {
    DisableNextLine,
    EnableNextLine,
    Disable,
    Enable,
    DisableFile,
}

impl DirectiveKind {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "pgweave-disable-next-line" => Some(Self::DisableNextLine),
            "pgweave-enable-next-line" => Some(Self::EnableNextLine),
            "pgweave-disable" => Some(Self::Disable),
            "pgweave-enable" => Some(Self::Enable),
            "pgweave-disable-file" => Some(Self::DisableFile),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Directive {
    pub kind: DirectiveKind,
    /// Targeted rules. Empty = all rules.
    pub rules: Vec<String>,
    /// Line (0-indexed) where the directive appears.
    pub line: usize,
}

/// An empty rule list means "all rules".
fn rules_match(rules: &[String], code: &str) -> bool {
    rules.is_empty() || rules.iter().any(|r| r == code)
}

impl Directive {
    /// Does this directive target `code`?
    pub fn targets(&self, code: &str) -> bool {
        rules_match(&self.rules, code)
    }
}

// ── Scan ──────────────────────────────────────────────────────────────────────

/// Set of active suppressions for a given source.
#[derive(Debug, Clone, Default)]
pub struct Suppressions {
    /// Cumulative line start offsets, to map byte offset → line.
    line_starts: Vec<usize>,
    /// `pgweave-disable-file` present (targeted rules; empty = all).
    file_disabled: Option<Vec<String>>,
    /// Per line: rules disabled via `disable-next-line`.
    next_line: HashMap<usize, Vec<String>>,
    /// Per line: rules re-enabled via `enable-next-line`.
    next_line_enabled: HashMap<usize, Vec<String>>,
    /// `[start, end)` line ranges coming from disable/enable blocks.
    blocks: Vec<(usize, usize, Vec<String>)>,
    /// All directives found, in order — useful to diagnose a useless or
    /// misspelled directive.
    pub directives: Vec<Directive>,
}

impl Suppressions {
    /// Scans the source and collects the directives.
    pub fn scan(source: &str) -> Self {
        let line_starts = compute_line_starts(source);
        let mut out = Self {
            line_starts,
            ..Default::default()
        };

        let mut open_block: Option<(usize, Vec<String>)> = None;
        let total_lines = source.lines().count();

        for (lineno, line) in source.lines().enumerate() {
            for (kind, rules) in directives_in_line(line) {
                out.directives.push(Directive {
                    kind,
                    rules: rules.clone(),
                    line: lineno,
                });
                match kind {
                    DirectiveKind::DisableFile => {
                        // The most permissive wins: "all rules" overrides a list.
                        match (&mut out.file_disabled, rules.is_empty()) {
                            (_, true) => out.file_disabled = Some(Vec::new()),
                            (Some(existing), false) if !existing.is_empty() => {
                                existing.extend(rules)
                            }
                            (Some(_), false) => {}
                            (None, false) => out.file_disabled = Some(rules),
                        }
                    }
                    DirectiveKind::DisableNextLine => {
                        out.next_line.entry(lineno + 1).or_default().extend(rules);
                    }
                    DirectiveKind::EnableNextLine => {
                        out.next_line_enabled
                            .entry(lineno + 1)
                            .or_default()
                            .extend(rules);
                    }
                    DirectiveKind::Disable => {
                        if open_block.is_none() {
                            open_block = Some((lineno, rules));
                        }
                    }
                    DirectiveKind::Enable => {
                        if let Some((start, block_rules)) = open_block.take() {
                            out.blocks.push((start, lineno, block_rules));
                        }
                    }
                }
            }
        }

        // A block left open runs to the end of the file.
        if let Some((start, rules)) = open_block {
            out.blocks.push((start, total_lines + 1, rules));
        }

        out
    }

    /// Line (0-indexed) containing the given byte offset.
    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    /// Is the diagnostic `code` located at `offset` suppressed?
    ///
    /// `offset` is `None` (diagnostic without a span): only suppressions
    /// covering the whole file apply, for lack of a position to compare.
    pub fn is_suppressed(&self, code: &str, offset: Option<usize>) -> bool {
        if let Some(rules) = &self.file_disabled
            && rules_match(rules, code)
        {
            return true;
        }

        let Some(offset) = offset else {
            return false;
        };
        let line = self.line_of(offset);

        // `enable-next-line` is more specific than the block: it wins.
        if let Some(rules) = self.next_line_enabled.get(&line)
            && rules_match(rules, code)
        {
            return false;
        }

        if let Some(rules) = self.next_line.get(&line)
            && rules_match(rules, code)
        {
            return true;
        }

        self.blocks
            .iter()
            .any(|(start, end, rules)| line > *start && line < *end && rules_match(rules, code))
    }

    /// Removes from `diags` everything covered by a directive.
    pub fn apply(&self, diags: &mut Vec<Diagnostic>) {
        diags.retain(|d| !self.is_suppressed(d.code, d.span.as_ref().map(|s| s.start)));
    }

    /// True if the source contains no directive — lets the common case skip
    /// the filtering work.
    pub fn is_empty(&self) -> bool {
        self.directives.is_empty()
    }
}

// ── Parsing a line ────────────────────────────────────────────────────────────

fn compute_line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Extracts the directives of a line. A line may carry several if it
/// contains several comments.
fn directives_in_line(line: &str) -> Vec<(DirectiveKind, Vec<String>)> {
    let mut found = Vec::new();

    for comment in comment_bodies(line) {
        let body = comment.trim();
        let mut parts = body.split_whitespace();
        let Some(head) = parts.next() else { continue };
        let Some(kind) = DirectiveKind::parse(head) else {
            continue;
        };

        // The rest = rule list, separated by commas and/or spaces.
        // A `--` ends the list: what follows is a free-form justification.
        let rest: Vec<&str> = parts.take_while(|t| *t != "--").collect();
        let rules: Vec<String> = rest
            .join(" ")
            .split(',')
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
            .map(|r| r.to_string())
            .collect();

        found.push((kind, rules));
    }

    found
}

/// Bodies of the comments present in a line (`-- …` and `/* … */`).
fn comment_bodies(line: &str) -> Vec<&str> {
    let mut out = Vec::new();

    if let Some(i) = line.find("--") {
        out.push(&line[i + 2..]);
    }

    let mut rest = line;
    while let Some(open) = rest.find("/*") {
        let after = &rest[open + 2..];
        match after.find("*/") {
            Some(close) => {
                out.push(&after[..close]);
                rest = &after[close + 2..];
            }
            None => {
                out.push(after);
                break;
            }
        }
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostic, Span};

    fn diag(code: &'static str, start: usize) -> Diagnostic {
        Diagnostic::error(code, "boom").with_span(Span {
            start,
            end: start + 1,
        })
    }

    /// Offset of the start of line `n` (0-indexed).
    fn line_offset(src: &str, n: usize) -> usize {
        compute_line_starts(src)[n]
    }

    #[test]
    fn disable_next_line_all_rules() {
        let src = "-- pgweave-disable-next-line\nSELECT bad;\nSELECT other;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("column-not-found", Some(line_offset(src, 1))));
        assert!(!s.is_suppressed("column-not-found", Some(line_offset(src, 2))));
    }

    #[test]
    fn disable_next_line_targeted_rule() {
        let src = "-- pgweave-disable-next-line column-not-found\nSELECT bad;";
        let s = Suppressions::scan(src);
        let off = Some(line_offset(src, 1));
        assert!(s.is_suppressed("column-not-found", off));
        assert!(
            !s.is_suppressed("table-not-found", off),
            "other rule untouched"
        );
    }

    #[test]
    fn several_rules_separated_by_comma() {
        let src = "-- pgweave-disable-next-line column-not-found, L001\nSELECT bad;";
        let s = Suppressions::scan(src);
        let off = Some(line_offset(src, 1));
        assert!(s.is_suppressed("column-not-found", off));
        assert!(s.is_suppressed("L001", off));
        assert!(!s.is_suppressed("L002", off));
    }

    #[test]
    fn justification_after_double_dash_ignored() {
        let src = "-- pgweave-disable-next-line L001 -- added by migration 42\nSELECT *;";
        let s = Suppressions::scan(src);
        let off = Some(line_offset(src, 1));
        assert!(s.is_suppressed("L001", off));
        assert!(
            !s.is_suppressed("migration", off),
            "the justification must not become a rule"
        );
    }

    #[test]
    fn disable_enable_block() {
        let src = "-- pgweave-disable\nSELECT a;\nSELECT b;\n-- pgweave-enable\nSELECT c;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("x", Some(line_offset(src, 1))));
        assert!(s.is_suppressed("x", Some(line_offset(src, 2))));
        assert!(
            !s.is_suppressed("x", Some(line_offset(src, 4))),
            "after enable, no more suppression"
        );
    }

    #[test]
    fn unclosed_block_runs_to_the_end() {
        let src = "SELECT a;\n-- pgweave-disable\nSELECT b;\nSELECT c;";
        let s = Suppressions::scan(src);
        assert!(!s.is_suppressed("x", Some(line_offset(src, 0))));
        assert!(s.is_suppressed("x", Some(line_offset(src, 2))));
        assert!(s.is_suppressed("x", Some(line_offset(src, 3))));
    }

    #[test]
    fn disable_file_covers_everything_even_without_span() {
        let src = "-- pgweave-disable-file\nSELECT a;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("x", Some(line_offset(src, 1))));
        assert!(
            s.is_suppressed("x", None),
            "a diagnostic without a span must be covered by disable-file"
        );
    }

    #[test]
    fn without_span_not_suppressed_by_disable_next_line() {
        let src = "-- pgweave-disable-next-line\nSELECT a;";
        let s = Suppressions::scan(src);
        assert!(
            !s.is_suppressed("x", None),
            "accepted limitation: no position, no targeted suppression"
        );
    }

    #[test]
    fn enable_next_line_wins_over_the_block() {
        let src = "-- pgweave-disable\nSELECT a;\n-- pgweave-enable-next-line L001\nSELECT b;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("L001", Some(line_offset(src, 1))));
        assert!(
            !s.is_suppressed("L001", Some(line_offset(src, 3))),
            "enable-next-line is more specific than the block"
        );
    }

    #[test]
    fn block_comment_recognized() {
        let src = "/* pgweave-disable-next-line L001 */\nSELECT *;";
        let s = Suppressions::scan(src);
        assert!(s.is_suppressed("L001", Some(line_offset(src, 1))));
    }

    #[test]
    fn apply_filters_the_list() {
        let src = "-- pgweave-disable-next-line L001\nSELECT *;\nSELECT *;";
        let s = Suppressions::scan(src);
        let mut diags = vec![
            diag("L001", line_offset(src, 1)),
            diag("L001", line_offset(src, 2)),
        ];
        s.apply(&mut diags);
        assert_eq!(diags.len(), 1, "only the second one must survive");
    }

    #[test]
    fn source_without_directive_is_empty() {
        let s = Suppressions::scan("SELECT 1;\n-- just a comment\n");
        assert!(s.is_empty());
        assert!(!s.is_suppressed("x", Some(0)));
    }

    #[test]
    fn misspelled_directive_ignored() {
        let s = Suppressions::scan("-- pgweave-disbale-next-line\nSELECT a;");
        assert!(s.is_empty(), "a typo must not suppress anything");
    }
}
