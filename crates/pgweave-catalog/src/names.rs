//! Noms que PostgreSQL choisit lui-même : contraintes et index non nommés,
//! séquences de `serial`, types tableau. Reproduits à l'identique
//! (`makeObjectName`, `ChooseConstraintName`, `makeArrayTypeName` dans les
//! sources de PostgreSQL), pour que le catalogue reconstruit soit celui de
//! la base.

/// `NAMEDATALEN - 1` : longueur maximale d'un identifiant, en octets.
pub const MAX_IDENT_BYTES: usize = 63;

/// Plus grande frontière de caractère ≤ `n` (`pg_mbcliplen`).
fn clip(s: &str, n: usize) -> &str {
    let mut n = n.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// `name1[_name2][_label]`, tronqué à 63 octets en raccourcissant d'abord la
/// plus longue des deux premières parties.
pub fn make_object_name(name1: &str, name2: Option<&str>, label: Option<&str>) -> String {
    let mut overhead = 0;
    if name2.is_some() {
        overhead += 1;
    }
    if let Some(l) = label {
        overhead += l.len() + 1;
    }
    let avail = MAX_IDENT_BYTES.saturating_sub(overhead);
    let mut n1 = name1.len();
    let mut n2 = name2.map_or(0, str::len);
    while n1 + n2 > avail {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let mut out = clip(name1, n1).to_string();
    if let Some(n) = name2 {
        out.push('_');
        out.push_str(clip(n, n2));
    }
    if let Some(l) = label {
        out.push('_');
        out.push_str(l);
    }
    out
}

/// Premier nom libre parmi `name1_name2_label`, `…_label1`, `…_label2`…
pub fn choose_name(
    name1: &str,
    name2: Option<&str>,
    label: &str,
    taken: impl Fn(&str) -> bool,
) -> String {
    let mut pass = 0;
    loop {
        let l = if pass == 0 {
            label.to_string()
        } else {
            format!("{label}{pass}")
        };
        let candidate = make_object_name(name1, name2, Some(&l));
        if !taken(&candidate) {
            return candidate;
        }
        pass += 1;
    }
}

/// Noms de colonnes joints par `_`, bornés à `NAMEDATALEN`
/// (`ChooseIndexNameAddition`).
pub fn columns_addition<'a>(columns: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for c in columns {
        if !out.is_empty() {
            out.push('_');
        }
        out.push_str(c);
        if out.len() >= MAX_IDENT_BYTES {
            break;
        }
    }
    clip(&out, MAX_IDENT_BYTES).to_string()
}

/// Nom du type tableau d'un type : `_nom`, tronqué, préfixé d'autant de `_`
/// que nécessaire pour être libre (`makeArrayTypeName`).
pub fn array_type_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut prefix = String::from("_");
    loop {
        let candidate = format!("{prefix}{}", clip(name, MAX_IDENT_BYTES - prefix.len()));
        if !taken(&candidate) {
            return candidate;
        }
        prefix.push('_');
        if prefix.len() >= MAX_IDENT_BYTES {
            return candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noms_courants() {
        assert_eq!(make_object_name("users", None, Some("pkey")), "users_pkey");
        assert_eq!(
            make_object_name("users", Some("email"), Some("key")),
            "users_email_key"
        );
        assert_eq!(
            choose_name("t", Some("a"), "key", |n| n == "t_a_key"),
            "t_a_key1"
        );
    }

    #[test]
    fn troncature_a_63_octets() {
        let long = "x".repeat(60);
        let n = make_object_name(&long, Some("colonne"), Some("fkey"));
        assert_eq!(n.len(), 63);
        assert!(n.ends_with("_colonne_fkey") || n.ends_with("_fkey"));
    }

    #[test]
    fn tableau() {
        assert_eq!(array_type_name("mood", |_| false), "_mood");
        assert_eq!(array_type_name("mood", |n| n == "_mood"), "__mood");
    }
}
