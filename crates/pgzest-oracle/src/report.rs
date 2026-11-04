use std::fmt::Write;

use crate::case::Case;
use crate::ours::OurDescription;
use crate::pg::PgDescription;

/// Results of a case: PostgreSQL and pgzest descriptions, query by query.
pub struct CaseReport {
    pub case: Case,
    pub pg: Vec<Result<PgDescription, String>>,
    pub ours: Vec<OurDescription>,
}

/// Global counters. A position (parameter or column) is "correct" when
/// pgzest gives exactly the same name and type as PostgreSQL.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Totals {
    pub queries: usize,
    /// Queries PostgreSQL rejects: a corpus case is then wrong.
    pub rejected_by_pg: usize,
    pub params: usize,
    pub params_ok: usize,
    pub columns: usize,
    pub columns_ok: usize,
}

/// Deterministic Markdown rendering of all cases.
pub fn render(reports: &[CaseReport], pg_major: u32) -> (String, Totals) {
    let mut totals = Totals::default();
    let mut body = String::new();

    for r in reports {
        let _ = writeln!(body, "\n## {}\n", r.case.name);
        for ((q, pg), ours) in r.case.queries.iter().zip(&r.pg).zip(&r.ours) {
            totals.queries += 1;
            let _ = writeln!(body, "### `{}`\n", q.name);
            match pg {
                Err(msg) => {
                    totals.rejected_by_pg += 1;
                    let _ = writeln!(body, "> **rejected by PostgreSQL**: {msg}\n");
                }
                Ok(pg) => render_query(&mut body, pg, ours, &mut totals),
            }
            for note in &ours.notes {
                let _ = writeln!(body, "> pgzest — {note}\n");
            }
        }
    }

    let mut out = String::new();
    let _ = writeln!(out, "# Differential oracle — PostgreSQL {pg_major}\n");
    let _ = writeln!(
        out,
        "Queries: {} · Matching columns: {}/{} ({} %) · Matching parameters: {}/{} ({} %)",
        totals.queries,
        totals.columns_ok,
        totals.columns,
        percent(totals.columns_ok, totals.columns),
        totals.params_ok,
        totals.params,
        percent(totals.params_ok, totals.params),
    );
    if totals.rejected_by_pg > 0 {
        let _ = writeln!(
            out,
            "\n**{} query(ies) rejected by PostgreSQL**: the corpus must be fixed.",
            totals.rejected_by_pg
        );
    }
    out.push_str(&body);
    (out, totals)
}

fn render_query(out: &mut String, pg: &PgDescription, ours: &OurDescription, t: &mut Totals) {
    // ── Parameters ──
    let pg_params = list(&pg.params);
    let (ours_params, ok) = match &ours.params {
        None => ("not inferred".to_string(), 0),
        Some(p) => {
            let ok = pg.params.iter().zip(p).filter(|(a, b)| a == b).count();
            (list(p), ok)
        }
    };
    t.params += pg.params.len();
    t.params_ok += ok;
    let _ = writeln!(
        out,
        "Parameters — PostgreSQL: {pg_params} · pgzest: {ours_params} · {ok}/{}\n",
        pg.params.len()
    );

    // ── Columns ──
    let ours_cols = ours.columns.as_deref();
    let rows = pg.columns.len().max(ours_cols.map(<[_]>::len).unwrap_or(0));
    if rows == 0 {
        let verdict = match ours_cols {
            Some(_) => "✓",
            None => "✗ (pgzest: not inferred)",
        };
        let _ = writeln!(out, "No output column {verdict}\n");
        return;
    }

    let _ = writeln!(out, "| # | PostgreSQL | pgzest | |\n|--:|---|---|:-:|");
    for i in 0..rows {
        let pg_col = pg.columns.get(i);
        let our_col = ours_cols.map(|c| c.get(i));
        let pg_cell = pg_col
            .map(|(n, ty)| format!("`{n} {ty}`"))
            .unwrap_or_else(|| "—".into());
        let our_cell = match our_col {
            None => "not inferred".to_string(),
            Some(None) => "—".to_string(),
            Some(Some((n, ty))) => {
                format!("`{} {ty}`", n.as_deref().unwrap_or("?column?"))
            }
        };
        let ok = matches!(
            (pg_col, our_col),
            (Some((pn, pt)), Some(Some((on, ot))))
                if on.as_deref().unwrap_or("?column?") == pn && ot == pt
        );
        if pg_col.is_some() {
            t.columns += 1;
            if ok {
                t.columns_ok += 1;
            }
        }
        let mark = if ok { "✓" } else { "✗" };
        let _ = writeln!(out, "| {} | {pg_cell} | {our_cell} | {mark} |", i + 1);
    }
    out.push('\n');
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items
            .iter()
            .map(|i| format!("`{i}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn percent(ok: usize, total: usize) -> usize {
    // Nothing to check: vacuously matching.
    (ok * 100).checked_div(total).unwrap_or(100)
}
