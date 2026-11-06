//! Migration header (PLAN.md, D9): the global context against which a
//! migration was generated and checked.
//!
//! ```sql
//! -- pgzest: postgres=18 extensions=btree_gist@1.7,citext@1.6 log=step
//! ```
//!
//! It goes among the leading lines of the file (comments and blank lines),
//! before the first statement. `postgres` is required, `extensions` optional;
//! each extension carries its version. `log` is the run log level the
//! routines of the migration were generated at (`[log]` of `pgzest.toml`),
//! `log=step:notify` for the `notify` target; absent, they are not logged. Any other form is an error: a misread
//! header would analyze the migration against a context other than the
//! announced one.

use std::fmt;
use std::ops::Range;

use pgzest_expand::{Instrumentation, LogLevel, LogTarget};

/// Prefix of a header line. Distinct from directives (`-- pgzest-disable…`),
/// which have no `:`.
const PREFIX: &str = "-- pgzest:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationHeader {
    /// PostgreSQL major version.
    pub postgres: u32,
    /// Active extensions, in written order.
    pub extensions: Vec<ExtensionRef>,
    /// Run log level of the generated routines.
    pub log: Instrumentation,
    /// Position of the line in the file (bytes, without the line break).
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRef {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderError {
    pub message: String,
    pub span: Range<usize>,
}

impl MigrationHeader {
    /// Header of a file: `None` if it has none, an error if it is malformed,
    /// duplicated or placed after the first statement.
    pub fn find(text: &str) -> Option<Result<MigrationHeader, HeaderError>> {
        let mut found: Option<MigrationHeader> = None;
        let mut in_head = true;
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let start = offset;
            offset += line.len();
            let content = line.trim_end_matches(['\n', '\r']);
            let span = start..start + content.len();
            let trimmed = content.trim_start();
            if !trimmed.starts_with(PREFIX) {
                if !(trimmed.is_empty() || trimmed.starts_with("--")) {
                    in_head = false;
                }
                continue;
            }
            if !in_head {
                return Some(Err(HeaderError {
                    message: "the `-- pgzest:` header must precede the first statement".into(),
                    span,
                }));
            }
            if found.is_some() {
                return Some(Err(HeaderError {
                    message: "duplicate `-- pgzest:` header".into(),
                    span,
                }));
            }
            match parse_fields(&trimmed[PREFIX.len()..]) {
                Ok((postgres, extensions, log)) => {
                    found = Some(MigrationHeader {
                        postgres,
                        extensions,
                        log,
                        span,
                    })
                }
                Err(message) => return Some(Err(HeaderError { message, span })),
            }
        }
        found.map(Ok)
    }
}

type Fields = (u32, Vec<ExtensionRef>, Instrumentation);

fn parse_fields(fields: &str) -> Result<Fields, String> {
    let mut postgres = None;
    let mut extensions: Option<Vec<ExtensionRef>> = None;
    let mut log = None;
    for field in fields.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            return Err(format!("field `{field}`: expected `key=value`"));
        };
        match key {
            "postgres" => {
                if postgres.is_some() {
                    return Err("duplicate `postgres`".into());
                }
                postgres = Some(value.parse::<u32>().map_err(|_| {
                    format!("`postgres={value}`: expected a major version (e.g. `postgres=18`)")
                })?);
            }
            "extensions" => {
                if extensions.is_some() {
                    return Err("duplicate `extensions`".into());
                }
                extensions = Some(parse_extensions(value)?);
            }
            "log" => {
                if log.is_some() {
                    return Err("duplicate `log`".into());
                }
                log = Some(parse_log(value)?);
            }
            other => return Err(format!("unknown field `{other}`")),
        }
    }
    let postgres = postgres.ok_or("missing `postgres` field")?;
    Ok((
        postgres,
        extensions.unwrap_or_default(),
        log.unwrap_or_default(),
    ))
}

fn parse_log(value: &str) -> Result<Instrumentation, String> {
    let expected = || {
        format!(
            "`log={value}`: expected `info`, `step` or `debug`, then optionally `:table` or `:notify`"
        )
    };
    let (level, target) = match value.split_once(':') {
        Some((l, t)) => (l, Some(t)),
        None => (value, None),
    };
    let log = LogLevel::parse(level)
        .filter(|l| *l != LogLevel::None)
        .ok_or_else(expected)?;
    let target = match target {
        Some(t) => LogTarget::parse(t).ok_or_else(expected)?,
        None => LogTarget::Table,
    };
    Ok(Instrumentation { log, target })
}

fn parse_extensions(value: &str) -> Result<Vec<ExtensionRef>, String> {
    let mut out: Vec<ExtensionRef> = Vec::new();
    for item in value.split(',') {
        let Some((name, version)) = item.split_once('@') else {
            return Err(format!("extension `{item}`: expected `name@version`"));
        };
        if name.is_empty() || version.is_empty() {
            return Err(format!("extension `{item}`: expected `name@version`"));
        }
        if out.iter().any(|e| e.name == name) {
            return Err(format!("duplicate extension `{name}`"));
        }
        out.push(ExtensionRef {
            name: name.to_string(),
            version: version.to_string(),
        });
    }
    Ok(out)
}

/// Header line, extensions sorted by name: the same input always yields the
/// same text.
impl fmt::Display for MigrationHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX} postgres={}", self.postgres)?;
        if !self.extensions.is_empty() {
            let mut exts: Vec<&ExtensionRef> = self.extensions.iter().collect();
            exts.sort_by(|a, b| a.name.cmp(&b.name));
            let list: Vec<String> = exts
                .iter()
                .map(|e| format!("{}@{}", e.name, e.version))
                .collect();
            write!(f, " extensions={}", list.join(","))?;
        }
        write!(f, "{}", LogField(self.log))
    }
}

/// ` log=…` of a header, empty when routines are not logged.
pub struct LogField(pub Instrumentation);

impl fmt::Display for LogField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Instrumentation { log, target } = self.0;
        if log == LogLevel::None {
            return Ok(());
        }
        write!(f, " log={}", log.as_str())?;
        if target != LogTarget::Table {
            write!(f, ":{}", target.as_str())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> MigrationHeader {
        MigrationHeader::find(text).expect("header").expect("valid")
    }

    fn err(text: &str) -> String {
        MigrationHeader::find(text)
            .expect("header")
            .expect_err("invalid")
            .message
    }

    #[test]
    fn full_header() {
        let text =
            "-- pgzest: postgres=18 extensions=citext@1.6,btree_gist@1.7\nCREATE TABLE t ();";
        let h = ok(text);
        assert_eq!(h.postgres, 18);
        assert_eq!(h.extensions.len(), 2);
        assert_eq!(h.extensions[0].name, "citext");
        assert_eq!(h.span, 0..text.find('\n').unwrap());
    }

    #[test]
    fn without_extensions_and_after_comments() {
        let h = ok("-- Migration 42\n\n-- pgzest: postgres=18\nSELECT 1;");
        assert_eq!(h.postgres, 18);
        assert!(h.extensions.is_empty());
    }

    #[test]
    fn absent() {
        assert!(MigrationHeader::find("-- pgzest-disable-file\nSELECT 1;").is_none());
    }

    #[test]
    fn deterministic_rendering() {
        let h = ok("-- pgzest: extensions=citext@1.6,btree_gist@1.7 postgres=18");
        assert_eq!(
            h.to_string(),
            "-- pgzest: postgres=18 extensions=btree_gist@1.7,citext@1.6"
        );
        assert_eq!(ok(&h.to_string()).extensions.len(), 2);
    }

    #[test]
    fn log_level() {
        let h = ok("-- pgzest: postgres=18 log=step");
        assert_eq!(h.log.log, LogLevel::Step);
        assert_eq!(h.log.target, LogTarget::Table);
        assert_eq!(h.to_string(), "-- pgzest: postgres=18 log=step");
        let h = ok("-- pgzest: log=debug:notify postgres=18");
        assert_eq!(h.log.target, LogTarget::Notify);
        assert_eq!(h.to_string(), "-- pgzest: postgres=18 log=debug:notify");
        assert_eq!(ok("-- pgzest: postgres=18").log.log, LogLevel::None);
        for bad in [
            "log=none",
            "log=trace",
            "log=step:file",
            "log=step log=info",
        ] {
            assert!(
                err(&format!("-- pgzest: postgres=18 {bad}")).contains("log"),
                "{bad}"
            );
        }
    }

    #[test]
    fn rejected_forms() {
        assert!(err("-- pgzest: extensions=citext@1.6").contains("postgres"));
        assert!(err("-- pgzest: postgres=eighteen").contains("major version"));
        assert!(err("-- pgzest: postgres=18 extensions=citext").contains("name@version"));
        assert!(err("-- pgzest: postgres=18 extensions=a@1,a@2").contains("duplicate"));
        assert!(err("-- pgzest: postgres=18 pg=17").contains("unknown"));
        assert!(err("-- pgzest: postgres=18\n-- pgzest: postgres=18").contains("duplicate"));
        assert!(err("SELECT 1;\n-- pgzest: postgres=18").contains("precede"));
    }
}
