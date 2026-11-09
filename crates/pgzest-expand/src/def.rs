//! Declarative definition of a macro, and loading from a file.
//!
//! Deliberately minimal format — six directives, a closed set of slot kinds.
//! Enough to express `@function` and `@trigger`, hence enough for most SQL
//! boilerplate, without becoming a template language.
//!
//! ```text
//! macro function
//! header {name:ident}({params:parens})[: {ret:type}]
//! default ret = VOID
//! option language = plpgsql -> LANGUAGE {}
//! flag immutable -> IMMUTABLE
//! flag security_definer -> SECURITY DEFINER
//! template
//! CREATE OR REPLACE FUNCTION {name}({params})
//! RETURNS {ret}
//! {options}
//! AS $$
//! BEGIN
//! {body}
//! END;
//! $$;
//! end
//! ```
//!
//! Directives:
//!
//! - `macro <name>` — the invoked name, here `@function`.
//! - `header <pattern>` — what follows `@name` up to the opening brace. Slots
//!   `{name:kind}`, optional groups between `[ ]`, the rest is literal.
//! - `default <slot> = <text>` — value when the slot is absent.
//! - `wrap <slot> -> <text with {}>` — rendering of an optional slot when
//!   present (`{}` receives its value, `\n` is a line break); absent, it
//!   renders nothing: `wrap when -> \nWHEN ({})`.
//! - `flag <word> -> <text>` — presence keyword, rendered in `{options}`.
//! - `option <word> = <default> -> <text with {}>` — valued keyword, rendered
//!   in `{options}`; `{}` receives the value.
//! - `template` … `end` — the template. `{slot}` interpolates, `{body}` inserts
//!   the body, `{options}` the rendered flags and options. Every slot of the
//!   header appears there: a captured slot never rendered would drop what
//!   the user wrote.
//!
//! Available slot kinds: see [`SlotKind`].

use std::collections::HashMap;

use crate::header::{HeaderPattern, SlotKind};

/// A presence flag: `immutable` → `IMMUTABLE`.
#[derive(Debug, Clone)]
pub struct Flag {
    pub keyword: String,
    pub emits: String,
}

/// A valued keyword: `language sql` → `LANGUAGE sql`.
#[derive(Debug, Clone)]
pub struct Opt {
    pub keyword: String,
    /// Value used when the keyword is absent. `None` = render nothing.
    pub default: Option<String>,
    /// Render template, `{}` receiving the value.
    pub emits: String,
}

/// A macro: how to recognize it, and what to translate it into.
#[derive(Debug, Clone)]
pub struct MacroDef {
    pub name: String,
    pub header: HeaderPattern,
    pub defaults: HashMap<String, String>,
    /// Rendering of optional slots when present, `{}` receiving the value.
    pub wraps: HashMap<String, String>,
    pub flags: Vec<Flag>,
    pub options: Vec<Opt>,
    pub template: String,
    /// Origin path, to locate a definition error.
    pub origin: String,
}

impl MacroDef {
    /// Parses a macro definition.
    pub fn parse(text: &str, origin: &str) -> Result<Self, String> {
        let mut name = None;
        let mut header = None;
        let mut defaults = HashMap::new();
        let mut wraps = HashMap::new();
        let mut flags = Vec::new();
        let mut options = Vec::new();
        let mut template: Option<String> = None;

        let mut lines = text.lines().enumerate().peekable();
        while let Some((lineno, raw)) = lines.next() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let err = |msg: &str| format!("{origin}:{}: {msg}", lineno + 1);

            if let Some(rest) = line.strip_prefix("macro ") {
                name = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("header ") {
                header = Some(HeaderPattern::parse(rest.trim()).map_err(|e| err(&e))?);
            } else if let Some(rest) = line.strip_prefix("default ") {
                let (k, v) = rest
                    .split_once('=')
                    .ok_or_else(|| err("`default` expects `<slot> = <text>`"))?;
                defaults.insert(k.trim().to_string(), v.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("wrap ") {
                let (k, v) = rest
                    .split_once("->")
                    .ok_or_else(|| err("`wrap` expects `<slot> -> <text with {}>`"))?;
                wraps.insert(k.trim().to_string(), v.trim().replace("\\n", "\n"));
            } else if let Some(rest) = line.strip_prefix("flag ") {
                let (kw, emits) = rest
                    .split_once("->")
                    .ok_or_else(|| err("`flag` expects `<word> -> <text>`"))?;
                flags.push(Flag {
                    keyword: kw.trim().to_lowercase(),
                    emits: emits.trim().to_string(),
                });
            } else if let Some(rest) = line.strip_prefix("option ") {
                let (lhs, emits) = rest
                    .split_once("->")
                    .ok_or_else(|| err("`option` expects `<word>[= <default>] -> <text>`"))?;
                let (kw, default) = match lhs.split_once('=') {
                    Some((k, d)) => (k.trim(), Some(d.trim().to_string())),
                    None => (lhs.trim(), None),
                };
                options.push(Opt {
                    keyword: kw.to_lowercase(),
                    default,
                    emits: emits.trim().to_string(),
                });
            } else if line == "template" {
                let mut body = String::new();
                let mut closed = false;
                for (_, tl) in lines.by_ref() {
                    if tl.trim() == "end" {
                        closed = true;
                        break;
                    }
                    body.push_str(tl);
                    body.push('\n');
                }
                if !closed {
                    return Err(err("`template` without `end`"));
                }
                template = Some(body);
            } else {
                return Err(err(&format!("unknown directive: {line:?}")));
            }
        }

        let def = Self {
            name: name.ok_or_else(|| format!("{origin}: missing `macro <name>` directive"))?,
            header: header.ok_or_else(|| format!("{origin}: missing `header` directive"))?,
            defaults,
            wraps,
            flags,
            options,
            template: template
                .ok_or_else(|| format!("{origin}: missing `template` … `end` block"))?,
            origin: origin.to_string(),
        };
        def.check_slots()?;
        Ok(def)
    }

    /// Every slot of the header is rendered by the template (the keywords
    /// through `{options}`), and every `wrap` names a slot.
    fn check_slots(&self) -> Result<(), String> {
        let slots = self.header.slots();
        for (slot, kind) in &slots {
            let placeholder = if *kind == SlotKind::Keywords {
                "{options}".to_string()
            } else {
                format!("{{{slot}}}")
            };
            if !self.template.contains(&placeholder) {
                return Err(format!(
                    "{}: slot `{slot}` is captured by the header but the template never renders it (`{placeholder}`)",
                    self.origin
                ));
            }
        }
        for slot in self.wraps.keys() {
            if !slots.iter().any(|(s, _)| s == slot) {
                return Err(format!(
                    "{}: `wrap {slot}`: the header has no slot `{slot}`",
                    self.origin
                ));
            }
        }
        Ok(())
    }

    /// Is the `body` slot expected, i.e. does the macro have a block?
    pub fn has_body(&self) -> bool {
        self.template.contains("{body}")
    }

    /// Renders `{options}`: options (with their defaults) then present flags.
    pub(crate) fn render_options(&self, present: &Keywords) -> String {
        let mut out: Vec<String> = Vec::new();

        for opt in &self.options {
            let value = present
                .values
                .get(&opt.keyword)
                .cloned()
                .or_else(|| opt.default.clone());
            if let Some(v) = value {
                out.push(opt.emits.replace("{}", &v));
            }
        }
        for flag in &self.flags {
            if present.flags.contains(&flag.keyword) {
                out.push(flag.emits.clone());
            }
        }
        out.join(" ")
    }
}

/// Keywords found in the header of an invocation.
#[derive(Debug, Default, Clone)]
pub struct Keywords {
    pub flags: Vec<String>,
    pub values: HashMap<String, String>,
}

impl Keywords {
    /// Splits a sequence of keywords into flags and key/value pairs.
    ///
    /// A word declared as `option` consumes the next word as its value; any
    /// other word is a flag.
    pub fn parse(text: &str, def: &MacroDef) -> Result<Self, String> {
        let mut out = Self::default();
        let words: Vec<&str> = text.split_whitespace().collect();
        let mut i = 0;
        while i < words.len() {
            let w = words[i].to_lowercase();
            if let Some(opt) = def.options.iter().find(|o| o.keyword == w) {
                let v = words
                    .get(i + 1)
                    .ok_or_else(|| format!("`{}` expects a value", opt.keyword))?;
                out.values.insert(w, (*v).to_string());
                i += 2;
            } else if def.flags.iter().any(|f| f.keyword == w) {
                out.flags.push(w);
                i += 1;
            } else {
                let known: Vec<&str> = def
                    .flags
                    .iter()
                    .map(|f| f.keyword.as_str())
                    .chain(def.options.iter().map(|o| o.keyword.as_str()))
                    .collect();
                return Err(format!(
                    "unknown keyword `{w}` for @{}; expected: {}",
                    def.name,
                    known.join(", ")
                ));
            }
        }
        Ok(out)
    }
}

/// The set of recognized macros: the bundled ones, plus the project's.
#[derive(Debug, Clone, Default)]
pub struct MacroSet {
    defs: HashMap<String, MacroDef>,
    /// Project fragments (`*.sql` of `[macros] directory`), by name.
    fragments: HashMap<String, crate::fragment::FragmentDef>,
    /// Log level and target of the PL/pgSQL routines (see
    /// [`crate::instrument`]).
    instrumentation: crate::instrument::Instrumentation,
}

impl MacroSet {
    /// The macros bundled with the tool.
    ///
    /// Defined in the same format as the user's, and embedded at compile time.
    /// An extension mechanism that the bundled macros do not use is an
    /// untested mechanism.
    pub fn builtin() -> Self {
        let mut set = Self::default();
        for (origin, text) in Self::builtin_sources() {
            let def = MacroDef::parse(text, origin)
                .unwrap_or_else(|e| panic!("invalid bundled macro: {e}"));
            set.defs.insert(def.name.clone(), def);
        }
        set
    }

    pub(crate) fn builtin_sources() -> &'static [(&'static str, &'static str)] {
        &[
            (
                "<builtin>/function.pgmacro",
                include_str!("builtins/function.pgmacro"),
            ),
            (
                "<builtin>/procedure.pgmacro",
                include_str!("builtins/procedure.pgmacro"),
            ),
            (
                "<builtin>/trigger.pgmacro",
                include_str!("builtins/trigger.pgmacro"),
            ),
        ]
    }

    /// Adds a definition. A user macro with the name of a bundled macro
    /// replaces it — the only way to adjust a bundled macro without forking
    /// the tool.
    pub fn insert(&mut self, def: MacroDef) {
        self.defs.insert(def.name.clone(), def);
    }

    /// Loads all `*.pgmacro` files and fragments (`*.sql`) of a directory,
    /// recursively.
    pub fn load_dir(&mut self, dir: &std::path::Path) -> Vec<String> {
        self.load_dir_with(dir, |_| None)
    }

    /// Like [`Self::load_dir`], with a file's text taken from `overlay` when it
    /// provides it (document open in the editor, unsaved).
    pub fn load_dir_with(
        &mut self,
        dir: &std::path::Path,
        overlay: impl Fn(&std::path::Path) -> Option<String>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        let mut files = Vec::new();
        collect_pgmacro(dir, &mut files);
        files.sort();
        for path in files {
            let text = match overlay(&path) {
                Some(t) => t,
                None => match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        errors.push(format!("{}: {e}", path.display()));
                        continue;
                    }
                },
            };
            let origin = path.display().to_string();
            if path.extension().is_some_and(|e| e == "sql") {
                let (defs, errs) = crate::fragment::parse_fragment_file(&text, &origin);
                errors.extend(errs);
                for d in defs {
                    if let Err(e) = self.insert_fragment(d) {
                        errors.push(e);
                    }
                }
                continue;
            }
            match MacroDef::parse(&text, &origin) {
                Ok(def) => self.insert(def),
                Err(e) => errors.push(e),
            }
        }
        for name in self.fragments.keys() {
            if self.defs.contains_key(name) {
                errors.push(format!(
                    "`@{name}` is both a macro and a fragment: rename one of them"
                ));
            }
        }
        errors
    }

    /// Adds a fragment; a name already taken is an error (no fragment silently
    /// replaces another).
    pub fn insert_fragment(&mut self, def: crate::fragment::FragmentDef) -> Result<(), String> {
        if crate::instrument::DIRECTIVES.contains(&def.name.as_str()) {
            return Err(format!(
                "{}:{}: `@{}` is a body directive: rename the fragment",
                def.origin, def.line, def.name
            ));
        }
        if let Some(prev) = self.fragments.get(&def.name) {
            return Err(format!(
                "{}:{}: fragment `{}` already defined at {}:{}",
                def.origin, def.line, def.name, prev.origin, prev.line
            ));
        }
        self.fragments.insert(def.name.clone(), def);
        Ok(())
    }

    pub fn fragment(&self, name: &str) -> Option<&crate::fragment::FragmentDef> {
        self.fragments.get(name)
    }

    pub(crate) fn fragments(&self) -> &HashMap<String, crate::fragment::FragmentDef> {
        &self.fragments
    }

    /// Fragment names, sorted.
    pub fn fragment_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.fragments.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// Logged build: the PL/pgSQL routines record their runs, and at the
    /// debug level `@snapshot` emits its copy instead of nothing. Never for
    /// analysis or migrations, whose routines stay as written.
    pub fn with_instrumentation(mut self, i: crate::instrument::Instrumentation) -> Self {
        self.instrumentation = i;
        self
    }

    pub fn instrumentation(&self) -> crate::instrument::Instrumentation {
        self.instrumentation.clone()
    }

    pub fn get(&self, name: &str) -> Option<&MacroDef> {
        self.defs.get(name)
    }

    /// Known names, sorted — for an error message or `pgzest macros`.
    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.defs.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// The slots declared by a macro, for documentation.
    pub fn slots_of(&self, name: &str) -> Option<Vec<(&str, SlotKind)>> {
        self.defs.get(name).map(|d| d.header.slots())
    }
}

fn collect_pgmacro(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_pgmacro(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("pgmacro" | "sql")
        ) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "\
macro greet
header {name:ident}
template
SELECT 'hello {name}';
end
";

    #[test]
    fn minimal_definition_parses() {
        let d = MacroDef::parse(SIMPLE, "t.pgmacro").expect("parse");
        assert_eq!(d.name, "greet");
        assert!(d.template.contains("hello {name}"));
        assert!(!d.has_body(), "no {{body}} in the template");
    }

    #[test]
    fn bundled_macros_are_valid() {
        // Safeguard: embedded macros must parse, otherwise `builtin()` would
        // panic at startup.
        let set = MacroSet::builtin();
        assert!(set.len() >= 3, "at least function, procedure, trigger");
        assert!(set.get("function").is_some());
        assert!(set.get("trigger").is_some());
    }

    #[test]
    fn unknown_directive_is_reported() {
        let e = MacroDef::parse("macro x\nheader {a:ident}\nblah\n", "t").unwrap_err();
        assert!(e.contains("unknown directive"), "got: {e}");
    }

    #[test]
    fn unclosed_template_is_reported() {
        let e = MacroDef::parse("macro x\nheader {a:ident}\ntemplate\nfoo\n", "t").unwrap_err();
        assert!(e.contains("without `end`"), "got: {e}");
    }

    #[test]
    fn mandatory_directives_are_checked() {
        assert!(
            MacroDef::parse("header {a:ident}\ntemplate\nx\nend\n", "t")
                .unwrap_err()
                .contains("missing `macro <name>`")
        );
        assert!(
            MacroDef::parse("macro x\ntemplate\nx\nend\n", "t")
                .unwrap_err()
                .contains("missing `header`")
        );
        assert!(
            MacroDef::parse("macro x\nheader {a:ident}\n", "t")
                .unwrap_err()
                .contains("`template`")
        );
    }

    #[test]
    fn a_slot_the_template_never_renders_is_refused() {
        let e = MacroDef::parse(
            "macro x\nheader {a:ident}[ when ({w:parens})]\ntemplate\n{a}\nend\n",
            "t",
        )
        .unwrap_err();
        assert!(e.contains("slot `w`"), "got: {e}");
        let e = MacroDef::parse(
            "macro x\nheader {a:ident}\nwrap b -> B {}\ntemplate\n{a}\nend\n",
            "t",
        )
        .unwrap_err();
        assert!(e.contains("no slot `b`"), "got: {e}");
    }

    #[test]
    fn flags_and_options_render() {
        let def = MacroDef::parse(
            "\
macro f
header {name:ident}
option language = plpgsql -> LANGUAGE {}
flag immutable -> IMMUTABLE
template
{name} {options}
end
",
            "t",
        )
        .unwrap();

        // Nothing given: only the option default is emitted.
        let none = Keywords::parse("", &def).unwrap();
        assert_eq!(def.render_options(&none), "LANGUAGE plpgsql");

        // Flag present + overridden option.
        let some = Keywords::parse("immutable language sql", &def).unwrap();
        assert_eq!(def.render_options(&some), "LANGUAGE sql IMMUTABLE");
    }

    #[test]
    fn unknown_keyword_is_rejected_with_the_list() {
        let def = MacroDef::parse(
            "macro f\nheader {n:ident}\nflag immutable -> IMMUTABLE\ntemplate\n{n}\nend\n",
            "t",
        )
        .unwrap();
        let e = Keywords::parse("imutable", &def).unwrap_err();
        assert!(e.contains("unknown"), "got: {e}");
        assert!(e.contains("immutable"), "must list the expected ones: {e}");
    }

    #[test]
    fn option_without_value_is_rejected() {
        let def = MacroDef::parse(
            "macro f\nheader {n:ident}\noption language -> LANGUAGE {}\ntemplate\n{n}\nend\n",
            "t",
        )
        .unwrap();
        assert!(
            Keywords::parse("language", &def)
                .unwrap_err()
                .contains("expects a value")
        );
    }

    #[test]
    fn user_macro_replaces_bundled_macro() {
        let mut set = MacroSet::builtin();
        let custom = MacroDef::parse(
            "macro function\nheader {n:ident}\ntemplate\n-- replaced {n}\nend\n",
            "mine.pgmacro",
        )
        .unwrap();
        set.insert(custom);
        assert_eq!(set.get("function").unwrap().origin, "mine.pgmacro");
    }
}
