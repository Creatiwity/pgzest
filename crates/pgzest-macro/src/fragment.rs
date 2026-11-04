//! Named fragments (PLAN.md, P6): a piece of SQL written once — column list,
//! expression, predicate — and inserted wherever it is needed.
//!
//! ```sql
//! -- fragment: search_vector(t)
//! -- Must stay identical to the GIN index expression.
//! to_tsvector('english'::regconfig, coalesce(t.name, '') || ' ' || coalesce(t.brand, ''))
//! ```
//!
//! - A `.sql` file of `[macros] directory` declares one or more of them, each
//!   under a `-- fragment: name` or `-- fragment: name(t, …)` header. The body
//!   is ordinary SQL; the comments following the header document it.
//! - `@name` inserts it, anywhere in a statement. Only a declared fragment
//!   name is an invocation: elsewhere, `@` remains PostgreSQL's absolute value
//!   operator.
//! - Parameters are relation **qualifiers**: `@name(e)` replaces each `t.` of
//!   the body with `e.`, `@name` without argument removes it (`t.col` →
//!   `col`). Nothing else is substituted: the format stays SQL, not a template
//!   language.
//! - A body that is a single non-atomic expression (operator, `IS`, `IN`,
//!   `BETWEEN`…) is inserted between parentheses, so that the surrounding
//!   operator does not split it; a list, an aliased item or an atom (column,
//!   call, literal) is inserted as is. The decision comes from the parser, not
//!   from a textual heuristic.
//! - A fragment may use another one; a cycle is an error.
//! - A fragment is not analyzed on its own: it is analyzed in each statement
//!   that uses it, after insertion, and an error there is reported at the
//!   invocation.

use std::collections::HashMap;

use crate::lex::{Kind, Token, tokenize};

/// A declared fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentDef {
    pub name: String,
    /// Qualifiers, in argument order.
    pub params: Vec<String>,
    /// Body SQL, without the documentation or surrounding whitespace.
    pub body: String,
    /// Comments between the header and the body, without `-- `.
    pub doc: Vec<String>,
    /// Definition file.
    pub origin: String,
    /// Header line (1-based).
    pub line: usize,
}

fn is_snake(s: &str) -> bool {
    let mut b = s.bytes();
    matches!(b.next(), Some(b'a'..=b'z' | b'_'))
        && b.all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// Parsed header: name and qualifiers.
type Header = (String, Vec<String>);

/// `name` or `name(a, b)`.
fn parse_header(payload: &str) -> Result<Header, String> {
    let payload = payload.trim();
    let (name, params) = match payload.split_once('(') {
        None => (payload, Vec::new()),
        Some((n, rest)) => {
            let inner = rest
                .trim_end()
                .strip_suffix(')')
                .ok_or("missing `)` after the qualifiers")?;
            let params: Vec<String> = inner
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if params.is_empty() {
                return Err("`()` without qualifier: write `-- fragment: name`".into());
            }
            (n.trim(), params)
        }
    };
    if !is_snake(name) {
        return Err(format!(
            "invalid fragment name: `{name}` (ASCII snake_case expected)"
        ));
    }
    for (i, p) in params.iter().enumerate() {
        if !is_snake(p) {
            return Err(format!("invalid qualifier: `{p}`"));
        }
        if params[..i].contains(p) {
            return Err(format!("qualifier `{p}` declared twice"));
        }
    }
    Ok((name.to_string(), params))
}

fn header_payload<'a>(src: &'a str, t: &Token) -> Option<&'a str> {
    src[t.range.clone()]
        .strip_prefix("--")?
        .trim_start()
        .strip_prefix("fragment:")
}

/// Fragments of a file, and the format errors (located as `file:line`).
pub fn parse_fragment_file(text: &str, origin: &str) -> (Vec<FragmentDef>, Vec<String>) {
    let line_of = |offset: usize| text[..offset].matches('\n').count() + 1;
    let (defs, errors) = parse_fragments(text, origin);
    let errors = errors
        .into_iter()
        .map(|(at, msg)| format!("{origin}:{}: {msg}", line_of(at)))
        .collect();
    (defs, errors)
}

/// Format errors of a fragment file, located in the text (editor).
pub fn fragment_file_diagnostics(text: &str) -> Vec<pgzest_diagnostics::Diagnostic> {
    parse_fragments(text, "")
        .1
        .into_iter()
        .map(|(at, msg)| {
            let end = text[at..].find('\n').map_or(text.len(), |k| at + k);
            pgzest_diagnostics::Diagnostic::error(crate::expand::MACRO_ERROR, msg)
                .with_span(pgzest_diagnostics::Span { start: at, end })
        })
        .collect()
}

fn parse_fragments(text: &str, origin: &str) -> (Vec<FragmentDef>, Vec<(usize, String)>) {
    let tokens = tokenize(text);
    let line_of = |offset: usize| text[..offset].matches('\n').count() + 1;
    let at_line_start = |offset: usize| {
        text[..offset]
            .rsplit('\n')
            .next()
            .is_none_or(|l| l.trim().is_empty())
    };

    let mut defs = Vec::new();
    let mut errors = Vec::new();
    // Headers: (token index, name, qualifiers).
    let mut headers: Vec<(usize, Result<Header, String>)> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if t.kind == Kind::LineComment
            && let Some(payload) = header_payload(text, t)
        {
            if at_line_start(t.range.start) {
                headers.push((i, parse_header(payload)));
            } else {
                errors.push((
                    t.range.start,
                    "a `-- fragment:` header must start the line".to_string(),
                ));
            }
        }
    }
    // SQL before the first header belongs to no fragment.
    let first = headers.first().map_or(tokens.len(), |(i, _)| *i);
    if let Some(t) = tokens[..first].iter().find(|t| {
        !matches!(
            t.kind,
            Kind::Whitespace | Kind::LineComment | Kind::BlockComment
        )
    }) {
        errors.push((
            t.range.start,
            "SQL outside a fragment (each fragment starts with `-- fragment: name`)".to_string(),
        ));
    }

    for (k, (hi, header)) in headers.iter().enumerate() {
        let at = tokens[*hi].range.start;
        let line = line_of(at);
        let (name, params) = match header {
            Ok(h) => h.clone(),
            Err(e) => {
                errors.push((at, e.clone()));
                continue;
            }
        };
        let end = headers.get(k + 1).map_or(tokens.len(), |(i, _)| *i);
        let span = &tokens[hi + 1..end];
        // Documentation: line comments before the first code token.
        let code: Vec<&Token> = span
            .iter()
            .filter(|t| {
                !matches!(
                    t.kind,
                    Kind::Whitespace | Kind::LineComment | Kind::BlockComment
                )
            })
            .collect();
        let (Some(first), Some(last)) = (code.first(), code.last()) else {
            errors.push((at, format!("fragment `{name}` is empty")));
            continue;
        };
        let doc = span
            .iter()
            .take_while(|t| t.range.start < first.range.start)
            .filter(|t| t.kind == Kind::LineComment)
            .map(|t| {
                let c = &text[t.range.clone()][2..];
                c.strip_prefix(' ').unwrap_or(c).trim_end().to_string()
            })
            .collect();
        if &text[last.range.clone()] == ";" {
            errors.push((
                last.range.start,
                "a fragment is not a statement: remove the `;`".to_string(),
            ));
            continue;
        }
        defs.push(FragmentDef {
            name,
            params,
            body: text[first.range.start..last.range.end].to_string(),
            doc,
            origin: origin.to_string(),
            line,
        });
    }
    (defs, errors)
}

/// Qualifier bound during rendering: given relation, or removed.
type Env = HashMap<String, Option<String>>;

/// An invocation recognized in a text.
pub(crate) struct Call<'a> {
    pub def: &'a FragmentDef,
    /// Written arguments (identifiers), empty without parentheses.
    pub args: Vec<String>,
    /// End of the invocation (after `)` or the name).
    pub end: usize,
}

/// Fragment invocation starting at token `i` (an `@`), if any: `Ok(None)` if
/// it is not a fragment name.
pub(crate) fn call_at<'a>(
    src: &str,
    tokens: &[Token],
    i: usize,
    fragments: &'a HashMap<String, FragmentDef>,
) -> Result<Option<(Call<'a>, usize)>, String> {
    if &src[tokens[i].range.clone()] != "@" {
        return Ok(None);
    }
    let Some(name_tok) = tokens
        .get(i + 1)
        .filter(|t| t.kind == Kind::Word && t.range.start == tokens[i].range.end)
    else {
        return Ok(None);
    };
    let Some(def) = fragments.get(&src[name_tok.range.clone()]) else {
        return Ok(None);
    };
    let mut next = i + 2;
    let mut args = Vec::new();
    if tokens
        .get(next)
        .is_some_and(|t| t.range.start == name_tok.range.end && &src[t.range.clone()] == "(")
    {
        next += 1;
        let mut expect_arg = true;
        loop {
            let Some(t) = tokens.get(next) else {
                return Err(format!("@{}: missing `)`", def.name));
            };
            next += 1;
            let text = &src[t.range.clone()];
            match t.kind {
                Kind::Whitespace => {}
                Kind::Word | Kind::QuotedIdent if expect_arg => {
                    args.push(text.to_string());
                    expect_arg = false;
                }
                Kind::Punct if text == "," && !expect_arg => expect_arg = true,
                Kind::Punct if text == ")" && (!expect_arg || args.is_empty()) => break,
                _ => {
                    return Err(format!(
                        "@{}: an argument is a relation name (alias, table), not `{text}`",
                        def.name
                    ));
                }
            }
        }
    }
    let end = tokens[next - 1].range.end;
    if !args.is_empty() && args.len() != def.params.len() {
        return Err(format!(
            "@{} expects {} qualifier(s) ({}) or none, {} given",
            def.name,
            def.params.len(),
            def.params.join(", "),
            args.len()
        ));
    }
    Ok(Some((Call { def, args, end }, next)))
}

/// SQL inserted for `call`: qualified body, nested fragments rendered,
/// parenthesized if needed.
pub(crate) fn render_call(
    call: &Call,
    env: &Env,
    fragments: &HashMap<String, FragmentDef>,
    stack: &mut Vec<String>,
) -> Result<String, String> {
    // An argument that is itself a qualifier of the enclosing fragment takes
    // its value.
    let mut bound: Env = HashMap::new();
    for (i, p) in call.def.params.iter().enumerate() {
        let v = match call.args.get(i) {
            None => None,
            Some(a) => match env.get(a) {
                Some(outer) => outer.clone(),
                None => Some(a.clone()),
            },
        };
        bound.insert(p.clone(), v);
    }
    let given = bound.values().filter(|v| v.is_some()).count();
    if given != 0 && given != call.def.params.len() {
        return Err(format!(
            "@{}: qualifiers partly removed, partly given",
            call.def.name
        ));
    }
    if stack.contains(&call.def.name) {
        return Err(format!(
            "recursive fragments: {} → {}",
            stack.join(" → "),
            call.def.name
        ));
    }
    stack.push(call.def.name.clone());
    let body = render_body(&call.def.body, &bound, fragments, stack);
    stack.pop();
    let body = body?;
    Ok(if needs_parens(&body) {
        format!("({body})")
    } else {
        body
    })
}

fn render_body(
    src: &str,
    env: &Env,
    fragments: &HashMap<String, FragmentDef>,
    stack: &mut Vec<String>,
) -> Result<String, String> {
    let tokens = tokenize(src);
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let text = &src[t.range.clone()];
        if let Some((call, next)) = call_at(src, &tokens, i, fragments)? {
            out.push_str(&render_call(&call, env, fragments, stack)?);
            i = next;
            continue;
        }
        // `t.` → `e.` or nothing, `t` being a qualifier; never after a `.`
        // (`s.t.col` denotes a table `t` of schema `s`).
        let qualifies = t.kind == Kind::Word
            && tokens
                .get(i + 1)
                .is_some_and(|n| &src[n.range.clone()] == ".")
            && (i == 0 || &src[tokens[i - 1].range.clone()] != ".");
        if qualifies && let Some(v) = env.get(text) {
            if let Some(rel) = v {
                out.push_str(rel);
                out.push('.');
            }
            i += 2;
            continue;
        }
        out.push_str(text);
        i += 1;
    }
    Ok(out)
}

/// Is the body a single expression whose meaning a neighboring operator could
/// change? Read by the parser: `SELECT <body>` with a single non-aliased item
/// that is not an atom.
fn needs_parens(body: &str) -> bool {
    use pgzest_ast::expr::ExprKind as K;
    let Ok(stmts) = pgzest_parser::parse(&format!("SELECT {body}")) else {
        return false;
    };
    let [pgzest_ast::Statement::Select(s)] = stmts.as_slice() else {
        return false;
    };
    let [pgzest_ast::SelectItem::Expr { expr, alias: None }] = s.columns.as_slice() else {
        return false;
    };
    !matches!(
        expr.kind,
        K::Literal(_)
            | K::Column(_)
            | K::FunctionCall { .. }
            | K::Cast { .. }
            | K::Case { .. }
            | K::Subquery(_)
            | K::Exists { .. }
            | K::Tuple(_)
            | K::FieldAccess { .. }
            | K::Parameter(_)
            | K::Interval(_)
            | K::Wildcard
            | K::QualifiedWildcard(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(defs: &str) -> HashMap<String, FragmentDef> {
        let (defs, errors) = parse_fragment_file(defs, "f.sql");
        assert!(errors.is_empty(), "{errors:?}");
        defs.into_iter().map(|d| (d.name.clone(), d)).collect()
    }

    fn expand(src: &str, fragments: &HashMap<String, FragmentDef>) -> Result<String, String> {
        render_body(src, &HashMap::new(), fragments, &mut Vec::new())
    }

    #[test]
    fn headers_documentation_and_body() {
        let (defs, errors) = parse_fragment_file(
            "-- Free preamble.\n\n\
             -- fragment: cols(t)\n-- The columns.\nt.a, t.b -- end\n\n\
             -- fragment: active\nstatus = 'active'\n",
            "f.sql",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "cols");
        assert_eq!(defs[0].params, vec!["t"]);
        assert_eq!(defs[0].doc, vec!["The columns."]);
        assert_eq!(defs[0].body, "t.a, t.b");
        assert_eq!(defs[0].line, 3);
        assert_eq!(defs[1].body, "status = 'active'");
    }

    #[test]
    fn format_errors() {
        for (src, expected) in [
            ("SELECT 1;\n-- fragment: x\na", "outside a fragment"),
            ("-- fragment: X\na", "invalid fragment name"),
            ("-- fragment: x(t, t)\na", "twice"),
            ("-- fragment: x\n", "empty"),
            ("-- fragment: x\nSELECT 1;", "remove the `;`"),
            ("SELECT 1 -- fragment: x\n", "start the line"),
        ] {
            let (_, errors) = parse_fragment_file(src, "f.sql");
            assert!(
                errors.iter().any(|e| e.contains(expected)),
                "{src:?}: {errors:?}"
            );
        }
    }

    #[test]
    fn qualifiers_substituted_or_removed() {
        let f = set("-- fragment: cols(t)\nt.a, t.\"B\", s.t.c, t");
        assert_eq!(
            expand("SELECT @cols(e)", &f).unwrap(),
            "SELECT e.a, e.\"B\", s.t.c, t"
        );
        assert_eq!(
            expand("SELECT @cols", &f).unwrap(),
            "SELECT a, \"B\", s.t.c, t"
        );
        assert_eq!(
            expand("SELECT @cols(\"My table\")", &f).unwrap(),
            "SELECT \"My table\".a, \"My table\".\"B\", s.t.c, t"
        );
    }

    #[test]
    fn expression_parenthesized_atom_and_list_inserted_as_is() {
        let f = set("-- fragment: p\na = 1 OR b = 2\n\
             -- fragment: f(t)\nlower(t.name)\n\
             -- fragment: l\na, b\n\
             -- fragment: alias\nlower(name) AS n\n\
             -- fragment: ordering\ncreated_at DESC");
        assert_eq!(
            expand("WHERE @p AND c", &f).unwrap(),
            "WHERE (a = 1 OR b = 2) AND c"
        );
        assert_eq!(expand("@f(x) = 'a'", &f).unwrap(), "lower(x.name) = 'a'");
        assert_eq!(expand("SELECT @l", &f).unwrap(), "SELECT a, b");
        assert_eq!(
            expand("SELECT @alias", &f).unwrap(),
            "SELECT lower(name) AS n"
        );
        assert_eq!(
            expand("ORDER BY @ordering", &f).unwrap(),
            "ORDER BY created_at DESC"
        );
    }

    #[test]
    fn nesting_and_cycle() {
        let f = set("-- fragment: inner(u)\nu.a\n\
             -- fragment: outer(t)\n@inner(t), t.b\n\
             -- fragment: x\n@y\n-- fragment: y\n@x");
        assert_eq!(expand("SELECT @outer(e)", &f).unwrap(), "SELECT e.a, e.b");
        assert_eq!(expand("SELECT @outer", &f).unwrap(), "SELECT a, b");
        let e = expand("SELECT @x", &f).unwrap_err();
        assert!(e.contains("recursive"), "{e}");
    }

    #[test]
    fn at_sign_outside_fragment_unchanged() {
        let f = set("-- fragment: cols\na");
        // Absolute value, unknown name, string, quoted identifier.
        let src = "SELECT @ -5, @x, '@cols', \"@cols\"";
        assert_eq!(expand(src, &f).unwrap(), src);
    }

    #[test]
    fn arguments_checked() {
        let f = set("-- fragment: j(a, b)\na.x = b.x");
        assert!(expand("@j(e)", &f).unwrap_err().contains("expects 2"));
        assert!(
            expand("@j(e, 1)", &f)
                .unwrap_err()
                .contains("relation name")
        );
        assert!(expand("@j(e", &f).unwrap_err().contains("missing"));
    }
}
