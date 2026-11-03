//! Code Rust des requêtes nommées, depuis leurs descripteurs (PLAN.md §5,
//! P5) : une fonction par requête, alignée sur sa cardinalité, une struct de
//! paramètres et une struct de ligne, exécutées par `pgweave-runtime`.
//!
//! Correspondance des types : table ci-dessous, extensible par projet
//! ([`RustOptions::overrides`], D9). Un type sans correspondance est un
//! refus, jamais un repli.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pgweave_catalog::ProjectCatalog;
use pgweave_catalog::model::TypeKind;
use pgweave_query::Descriptor;
use pgweave_query::descriptor::NullabilityDesc;
use pgweave_sema::{Ty, TypeSystem};

/// Crate de dates et heures du projet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Temporal {
    #[default]
    Time,
    Chrono,
}

#[derive(Debug, Clone)]
pub struct RustOptions {
    /// Chemin du runtime dans le crate utilisateur.
    pub runtime: String,
    pub temporal: Temporal,
    /// Type Rust d'un type PostgreSQL (`"public.geometry"` →
    /// `"geo_types::Geometry"`), prioritaire sur la table intégrée.
    pub overrides: BTreeMap<String, String>,
}

impl Default for RustOptions {
    fn default() -> Self {
        RustOptions {
            runtime: "pgweave_runtime".into(),
            temporal: Temporal::default(),
            overrides: BTreeMap::new(),
        }
    }
}

/// Type Rust d'une valeur : possédée (colonne) et empruntée (paramètre).
#[derive(Debug, Clone)]
struct RustTy {
    owned: String,
    borrowed: String,
    /// La forme empruntée porte la durée de vie `'a`.
    lifetime: bool,
}

impl RustTy {
    fn copy(t: &str) -> Self {
        RustTy {
            owned: t.into(),
            borrowed: t.into(),
            lifetime: false,
        }
    }
}

struct Generator<'c> {
    ts: TypeSystem<'c>,
    opts: &'c RustOptions,
    /// Enums utilisés : nom qualifié → (type Rust, libellés).
    enums: BTreeMap<Ty, (String, String, Vec<String>)>,
}

/// Module Rust des requêtes `descriptors`, typées contre `catalog`. Les
/// erreurs disent quelle requête ne se traduit pas, et pourquoi.
pub fn generate_rust(
    descriptors: &[Descriptor],
    catalog: &ProjectCatalog,
    opts: &RustOptions,
) -> Result<String, Vec<String>> {
    let mut g = Generator {
        ts: TypeSystem::new(&catalog.catalog),
        opts,
        enums: BTreeMap::new(),
    };
    let mut errors = Vec::new();
    let mut body = String::new();
    for d in descriptors {
        if let Err(e) = pgweave_query::check_row_reference(d, descriptors) {
            errors.push(format!("{} : {e}", d.name));
            continue;
        }
        match g.query(d) {
            Ok(code) => body.push_str(&code),
            Err(e) => errors.push(format!("{} : {e}", d.name)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut out = String::from(
        "// Généré par pgweave à partir des descripteurs de requêtes. Ne pas modifier.\n\n",
    );
    let mut names = BTreeMap::new();
    for (ty, (rust, typname, labels)) in &g.enums {
        if let Some(other) = names.insert(rust.clone(), ty.clone()) {
            return Err(vec![format!(
                "les enums {other} et {ty} donnent le même type Rust `{rust}`"
            )]);
        }
        out.push_str(&enum_code(&g.opts.runtime, rust, typname, labels)?);
    }
    out.push_str(&body);
    Ok(out)
}

impl Generator<'_> {
    fn query(&mut self, d: &Descriptor) -> Result<String, String> {
        if let Some(copy) = &d.copy {
            return self.copy(d, copy);
        }
        let rt = self.opts.runtime.clone();
        let fn_name = rust_ident(&d.name)?;
        let pascal = pascal(&d.name);
        let const_name = d.name.to_uppercase();
        let mut o = String::new();

        // OID de chaque paramètre ; un domaine est passé sous son type de
        // base (le serveur convertit implicitement), un type sans OID fixe
        // laisse le serveur typer la requête.
        let mut oids = Some(Vec::new());
        for p in &d.params {
            let base = self.ts.base_type(&qname(&p.ty.pg));
            match (self.ts.def(&base).and_then(|t| t.oid), oids.as_mut()) {
                (Some(oid), Some(v)) => v.push(oid),
                _ => oids = None,
            }
        }
        let oids = match oids {
            Some(v) => format!(
                "Some(&[{}])",
                v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            ),
            None => "None".into(),
        };
        let _ = writeln!(o, "/// `{}`", d.sql.replace('\n', " "));
        let _ = writeln!(
            o,
            "pub const {const_name}: {rt}::Query = {rt}::Query {{ name: {:?}, sql: {:?}, param_oids: {oids} }};\n",
            d.name, d.sql
        );

        // Paramètres.
        let mut params = Vec::new();
        let mut lifetime = false;
        for p in &d.params {
            let t = self.rust_type(&p.ty.pg)?;
            lifetime |= t.lifetime;
            let ty = match p.nullable {
                // Un paramètre de nullabilité non établie est attendu non
                // nul : NULL n'a de sens que là où la requête l'accepte
                // explicitement (colonne nullable).
                NullabilityDesc::Nullable => format!("Option<{}>", t.borrowed),
                NullabilityDesc::NonNull | NullabilityDesc::Unknown => t.borrowed,
            };
            params.push((field_ident(&p.name)?, ty));
        }
        let params_ty = if params.is_empty() {
            None
        } else {
            let name = format!("{pascal}Params");
            let lt = if lifetime { "<'a>" } else { "" };
            let _ = writeln!(o, "#[derive(Debug, Clone)]\npub struct {name}{lt} {{");
            for (f, t) in &params {
                let _ = writeln!(o, "    pub {f}: {t},");
            }
            let _ = writeln!(o, "}}\n");
            Some(format!("{name}{}", if lifetime { "<'_>" } else { "" }))
        };

        // Ligne : celle d'une autre requête (`-> autre`, vérifiée), ou la
        // sienne.
        let row_ty = if let Some(r) = &d.row {
            Some(format!("{}Row", self::pascal(r)))
        } else if d.columns.is_empty() {
            None
        } else {
            let name = format!("{pascal}Row");
            let mut seen = std::collections::BTreeSet::new();
            let mut fields = Vec::new();
            for c in &d.columns {
                let f = field_ident(&c.name)
                    .map_err(|e| format!("{e} (colonne de sortie : lui donner un alias)"))?;
                if !seen.insert(f.clone()) {
                    return Err(format!(
                        "deux colonnes de sortie s'appellent `{}` : leur donner des alias distincts",
                        c.name
                    ));
                }
                let t = self.rust_type(&c.ty.pg)?;
                let ty = match c.nullable {
                    NullabilityDesc::NonNull => t.owned,
                    NullabilityDesc::Nullable | NullabilityDesc::Unknown => {
                        format!("Option<{}>", t.owned)
                    }
                };
                fields.push((f, ty));
            }
            let _ = writeln!(
                o,
                "#[derive(Debug, Clone, PartialEq)]\npub struct {name} {{"
            );
            for (f, t) in &fields {
                let _ = writeln!(o, "    pub {f}: {t},");
            }
            let _ = writeln!(o, "}}\n");
            let _ = writeln!(o, "impl {name} {{");
            let _ = writeln!(
                o,
                "    fn from_row(row: &{rt}::Row) -> Result<Self, {rt}::tokio_postgres::Error> {{"
            );
            let _ = writeln!(o, "        Ok(Self {{");
            for (i, (f, _)) in fields.iter().enumerate() {
                let _ = writeln!(o, "            {f}: row.try_get({i})?,");
            }
            let _ = writeln!(o, "        }})\n    }}\n}}\n");
            Some(name)
        };

        // Fonction.
        let args = match &params_ty {
            Some(t) => format!(", params: {t}"),
            None => String::new(),
        };
        // Un domaine passe par `Domain` : le serveur peut l'annoncer tel quel.
        let domains: Vec<bool> = d.params.iter().map(|p| self.is_domain(&p.ty.pg)).collect();
        let bound = match &params_ty {
            Some(_) => format!(
                "&[{}]",
                params
                    .iter()
                    .zip(&domains)
                    .map(|((f, _), dom)| if *dom {
                        format!("&{rt}::Domain(params.{f})")
                    } else {
                        format!("&params.{f}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "&[]".into(),
        };
        use pgweave_query::Cardinality;
        let (ret, call) = match (d.cardinality, &row_ty) {
            (Cardinality::None, _) | (_, None) => (
                "u64".to_string(),
                format!("{rt}::execute(client, &{const_name}, {bound})"),
            ),
            (Cardinality::One, Some(r)) => (
                r.clone(),
                format!("{rt}::fetch_one(client, &{const_name}, {bound}, {r}::from_row)"),
            ),
            (Cardinality::Optional, Some(r)) => (
                format!("Option<{r}>"),
                format!("{rt}::fetch_optional(client, &{const_name}, {bound}, {r}::from_row)"),
            ),
            (Cardinality::Many, Some(r)) => (
                format!("Vec<{r}>"),
                format!("{rt}::fetch_all(client, &{const_name}, {bound}, {r}::from_row)"),
            ),
        };
        let _ = writeln!(
            o,
            "pub async fn {fn_name}<C: {rt}::GenericClient + ?Sized>(client: &C{args}) -> Result<{ret}, {rt}::Error> {{\n    {call}.await\n}}\n"
        );
        // Plusieurs lignes : aussi en flux, décodées à leur arrivée.
        if let (Cardinality::Many, Some(r)) = (d.cardinality, &row_ty) {
            let _ = writeln!(
                o,
                "/// Comme [`{fn_name}`], les lignes décodées au fil de leur arrivée.\npub async fn {fn_name}_stream<C: {rt}::GenericClient + ?Sized>(client: &C{args}) -> Result<impl {rt}::futures_util::Stream<Item = Result<{r}, {rt}::Error>> + Send + use<C>, {rt}::Error> {{\n    {rt}::stream(client, &{const_name}, {bound}, {r}::from_row).await\n}}\n"
            );
        }
        Ok(o)
    }

    /// `COPY … FROM STDIN` : enregistrements typés (binaire) ou flux
    /// d'octets (texte, CSV).
    fn copy(
        &mut self,
        d: &Descriptor,
        copy: &pgweave_query::descriptor::CopyDesc,
    ) -> Result<String, String> {
        let rt = self.opts.runtime.clone();
        let fn_name = rust_ident(&d.name)?;
        let const_name = d.name.to_uppercase();
        let mut oids = Some(Vec::new());
        for c in &d.columns {
            let base = self.ts.base_type(&qname(&c.ty.pg));
            match (self.ts.def(&base).and_then(|t| t.oid), oids.as_mut()) {
                (Some(oid), Some(v)) => v.push(oid),
                _ => oids = None,
            }
        }
        let oids = match oids {
            Some(v) => format!(
                "Some(&[{}])",
                v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            ),
            None => "None".into(),
        };
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let table = qname(&copy.table);
        let table = format!("{}.{}", quote(&table.schema), quote(&table.name));
        let columns = d
            .columns
            .iter()
            .map(|c| quote(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut o = String::new();
        let _ = writeln!(o, "/// `{}`", d.sql.replace('\n', " "));
        let _ = writeln!(
            o,
            "pub const {const_name}: {rt}::CopyIn = {rt}::CopyIn {{ name: {:?}, sql: {:?}, table: {table:?}, columns: {columns:?}, column_oids: {oids} }};\n",
            d.name, d.sql
        );
        if copy.format != "binary" {
            let _ = writeln!(
                o,
                "/// Données au format {} lues dans `reader`, telles quelles ; renvoie le nombre de lignes copiées.\npub async fn {fn_name}<C: {rt}::CopyClient + ?Sized, R: {rt}::tokio::io::AsyncRead + Unpin>(client: &C, reader: R) -> Result<u64, {rt}::Error> {{\n    {rt}::copy_in_stream(client, &{const_name}, reader).await\n}}\n",
                copy.format
            );
            return Ok(o);
        }
        let record = format!("{}Record", pascal(&d.name));
        let mut fields = Vec::new();
        let mut lifetime = false;
        for c in &d.columns {
            let t = self.rust_type(&c.ty.pg)?;
            lifetime |= t.lifetime;
            let ty = match c.nullable {
                NullabilityDesc::NonNull => t.borrowed,
                _ => format!("Option<{}>", t.borrowed),
            };
            fields.push((field_ident(&c.name)?, ty));
        }
        let lt = if lifetime { "<'a>" } else { "" };
        let _ = writeln!(o, "#[derive(Debug, Clone)]\npub struct {record}{lt} {{");
        for (f, t) in &fields {
            let _ = writeln!(o, "    pub {f}: {t},");
        }
        let _ = writeln!(o, "}}\n");
        let item = if lifetime {
            format!("{record}<'a>")
        } else {
            record.clone()
        };
        let values = fields
            .iter()
            .map(|(f, _)| format!("&r.{f}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            o,
            "/// Copie binaire des enregistrements ; renvoie le nombre de lignes copiées.\npub async fn {fn_name}<'a, C: {rt}::CopyClient + ?Sized>(client: &C, records: impl IntoIterator<Item = {item}>) -> Result<u64, {rt}::Error> {{\n    let mut copy = {rt}::BinaryCopy::open(client, &{const_name}).await?;\n    for r in records {{\n        copy.write(&[{values}]).await?;\n    }}\n    copy.finish().await\n}}\n"
        );
        Ok(o)
    }

    /// Le type (ou l'élément de tableau) est un domaine.
    fn is_domain(&self, qualified: &str) -> bool {
        matches!(
            self.ts.def(&qname(qualified)).map(|d| &d.kind),
            Some(TypeKind::Domain { .. })
        )
    }

    /// Extension qui apporte ce type, s'il en vient une.
    fn extension_of(&self, ty: &Ty) -> Option<String> {
        let cat = self.ts.catalog;
        cat.extension_packs.iter().find_map(|(ext, pack)| {
            let schema = &cat.extensions.get(ext)?.schema;
            (*schema == ty.schema && pack.types.iter().any(|t| t.name.name == ty.name))
                .then(|| ext.clone())
        })
    }

    /// Type Rust d'un type PostgreSQL (nom qualifié du descripteur).
    fn rust_type(&mut self, qualified: &str) -> Result<RustTy, String> {
        if let Some(t) = self.opts.overrides.get(qualified) {
            return Ok(RustTy::copy(t));
        }
        let ty = qname(qualified);
        let def = self.ts.def(&ty);
        match def.map(|d| &d.kind) {
            Some(TypeKind::Domain { .. }) => {
                let base = self.ts.base_type(&ty);
                return self.rust_type(&base.to_string());
            }
            Some(TypeKind::Enum { labels }) => {
                let rust = pascal(&ty.name);
                self.enums
                    .insert(ty.clone(), (rust.clone(), ty.name.clone(), labels.clone()));
                return Ok(RustTy::copy(&rust));
            }
            Some(TypeKind::Array { element }) => {
                let e = self.rust_type(&element.to_string())?;
                return Ok(RustTy {
                    // Un élément de tableau peut être NULL.
                    owned: format!("Vec<Option<{}>>", e.owned),
                    borrowed: format!("&'a [{}]", e.owned),
                    lifetime: true,
                });
            }
            _ => {}
        }
        if ty.schema != "pg_catalog" {
            // Type d'une extension : correspondance par extension et nom,
            // quel que soit le schéma où elle est installée (D9).
            return match (self.extension_of(&ty).as_deref(), ty.name.as_str()) {
                (Some("citext"), "citext") => Ok(RustTy {
                    owned: "String".into(),
                    borrowed: "&'a str".into(),
                    lifetime: true,
                }),
                (Some(ext), _) => Err(format!(
                    "pas de type Rust pour {qualified} (extension {ext}) : le déclarer dans la table de correspondance du projet"
                )),
                (None, _) => Err(format!(
                    "pas de type Rust pour {qualified} : le déclarer dans la table de correspondance du projet"
                )),
            };
        }
        let time = self.opts.temporal == Temporal::Time;
        Ok(match ty.name.as_str() {
            "bool" => RustTy::copy("bool"),
            "int2" => RustTy::copy("i16"),
            "int4" => RustTy::copy("i32"),
            "int8" => RustTy::copy("i64"),
            "float4" => RustTy::copy("f32"),
            "float8" => RustTy::copy("f64"),
            "oid" => RustTy::copy("u32"),
            "char" => RustTy::copy("i8"),
            "text" | "varchar" | "bpchar" | "name" => RustTy {
                owned: "String".into(),
                borrowed: "&'a str".into(),
                lifetime: true,
            },
            "bytea" => RustTy {
                owned: "Vec<u8>".into(),
                borrowed: "&'a [u8]".into(),
                lifetime: true,
            },
            "uuid" => RustTy::copy("uuid::Uuid"),
            "json" | "jsonb" => RustTy {
                owned: "serde_json::Value".into(),
                borrowed: "&'a serde_json::Value".into(),
                lifetime: true,
            },
            "numeric" => RustTy::copy("rust_decimal::Decimal"),
            "date" if time => RustTy::copy("time::Date"),
            "time" if time => RustTy::copy("time::Time"),
            "timestamp" if time => RustTy::copy("time::PrimitiveDateTime"),
            "timestamptz" if time => RustTy::copy("time::OffsetDateTime"),
            "date" => RustTy::copy("chrono::NaiveDate"),
            "time" => RustTy::copy("chrono::NaiveTime"),
            "timestamp" => RustTy::copy("chrono::NaiveDateTime"),
            "timestamptz" => RustTy::copy("chrono::DateTime<chrono::Utc>"),
            _ => {
                return Err(format!(
                    "pas de type Rust pour {qualified} : le déclarer dans la table de correspondance du projet"
                ));
            }
        })
    }
}

/// Pièces d'une requête inline (`query!`), à placer dans un bloc.
pub struct InlineQuery {
    /// `const QUERY: Query = …;`
    pub query_const: String,
    /// Struct `Row` et son décodage, si la requête renvoie des lignes.
    pub row: Option<String>,
    /// Paramètres dans l'ordre : nom, type Rust attendu (sans durée de vie
    /// nommée : `&str`, `Option<i64>`…), et s'il est d'un domaine (à passer
    /// par `Domain`).
    pub params: Vec<(String, String, bool)>,
    /// Marqueur de cardinalité (`One`, `Optional`, `Many`, `NoRows`).
    pub cardinality: &'static str,
}

/// Requête inline : comme une requête nommée, sans enum (ses types Rust
/// doivent être uniques dans le crate : requêtes nommées, `build.rs`).
pub fn inline_query(
    d: &Descriptor,
    catalog: &ProjectCatalog,
    opts: &RustOptions,
) -> Result<InlineQuery, String> {
    if d.copy.is_some() {
        return Err("COPY : déclarer une requête nommée (build.rs)".into());
    }
    let mut g = Generator {
        ts: TypeSystem::new(&catalog.catalog),
        opts,
        enums: BTreeMap::new(),
    };
    let rt = &opts.runtime;
    let mut oids = Some(Vec::new());
    let mut params = Vec::new();
    for p in &d.params {
        let base = g.ts.base_type(&qname(&p.ty.pg));
        match (g.ts.def(&base).and_then(|t| t.oid), oids.as_mut()) {
            (Some(oid), Some(v)) => v.push(oid),
            _ => oids = None,
        }
        let t = g.rust_type(&p.ty.pg)?;
        let ty = t.borrowed.replace("&'a ", "&");
        let ty = match p.nullable {
            NullabilityDesc::Nullable => format!("Option<{ty}>"),
            NullabilityDesc::NonNull | NullabilityDesc::Unknown => ty,
        };
        params.push((p.name.clone(), ty, g.is_domain(&p.ty.pg)));
    }
    let oids = match oids {
        Some(v) => format!(
            "Some(&[{}])",
            v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
        ),
        None => "None".into(),
    };
    let query_const = format!(
        "const QUERY: {rt}::Query = {rt}::Query {{ name: {:?}, sql: {:?}, param_oids: {oids} }};",
        d.name, d.sql
    );
    let row = if d.columns.is_empty() {
        None
    } else {
        let mut o = String::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut fields = Vec::new();
        for c in &d.columns {
            let f = field_ident(&c.name)
                .map_err(|e| format!("{e} (colonne de sortie : lui donner un alias)"))?;
            if !seen.insert(f.clone()) {
                return Err(format!(
                    "deux colonnes de sortie s'appellent `{}` : leur donner des alias distincts",
                    c.name
                ));
            }
            let t = g.rust_type(&c.ty.pg)?;
            fields.push((
                f,
                match c.nullable {
                    NullabilityDesc::NonNull => t.owned,
                    _ => format!("Option<{}>", t.owned),
                },
            ));
        }
        let _ = writeln!(o, "#[derive(Debug, Clone, PartialEq)]\npub struct Row {{");
        for (f, t) in &fields {
            let _ = writeln!(o, "    pub {f}: {t},");
        }
        let _ = writeln!(
            o,
            "}}\nimpl Row {{ fn from_row(row: &{rt}::Row) -> Result<Self, {rt}::tokio_postgres::Error> {{ Ok(Self {{"
        );
        for (i, (f, _)) in fields.iter().enumerate() {
            let _ = writeln!(o, "    {f}: row.try_get({i})?,");
        }
        let _ = writeln!(o, "}}) }} }}");
        Some(o)
    };
    if let Some((ty, _)) = g.enums.iter().next() {
        return Err(format!(
            "l'enum {ty} n'a pas de type Rust dans une requête inline : utiliser une requête nommée (build.rs), qui génère ses enums"
        ));
    }
    use pgweave_query::Cardinality;
    let cardinality = match (d.cardinality, &row) {
        (Cardinality::None, _) | (_, None) => "NoRows",
        (Cardinality::One, _) => "One",
        (Cardinality::Optional, _) => "Optional",
        (Cardinality::Many, _) => "Many",
    };
    Ok(InlineQuery {
        query_const,
        row,
        params,
        cardinality,
    })
}

/// Enum Rust d'un enum PostgreSQL, encodé par ses libellés.
fn enum_code(
    rt: &str,
    rust: &str,
    typname: &str,
    labels: &[String],
) -> Result<String, Vec<String>> {
    let mut variants = Vec::new();
    for l in labels {
        let v = pascal(l);
        if v.is_empty() || !v.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            return Err(vec![format!(
                "libellé {l:?} de l'enum {typname} : pas de variante Rust possible"
            )]);
        }
        if variants.iter().any(|(x, _)| *x == v) {
            return Err(vec![format!(
                "libellés de l'enum {typname} : deux donnent la variante `{v}`"
            )]);
        }
        variants.push((v, l.clone()));
    }
    let mut o = String::new();
    let _ = writeln!(
        o,
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\npub enum {rust} {{"
    );
    for (v, l) in &variants {
        let _ = writeln!(o, "    /// `{l}`\n    {v},");
    }
    let _ = writeln!(o, "}}\n");
    let _ = writeln!(o, "impl {rt}::PgEnum for {rust} {{");
    let _ = writeln!(o, "    const TYPE_NAME: &'static str = {typname:?};");
    let _ = writeln!(
        o,
        "    fn label(&self) -> &'static str {{\n        match self {{"
    );
    for (v, l) in &variants {
        let _ = writeln!(o, "            {rust}::{v} => {l:?},");
    }
    let _ = writeln!(o, "        }}\n    }}");
    let _ = writeln!(
        o,
        "    fn from_label(label: &str) -> Option<Self> {{\n        match label {{"
    );
    for (v, l) in &variants {
        let _ = writeln!(o, "            {l:?} => Some({rust}::{v}),");
    }
    let _ = writeln!(o, "            _ => None,\n        }}\n    }}\n}}\n");
    let _ = writeln!(
        o,
        "impl<'a> {rt}::FromSql<'a> for {rust} {{
    fn from_sql(_: &{rt}::Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {{
        {rt}::enum_from_sql(raw)
    }}
    fn accepts(ty: &{rt}::Type) -> bool {{
        ty.name() == {typname:?}
    }}
}}

impl {rt}::ToSql for {rust} {{
    fn to_sql(&self, _: &{rt}::Type, out: &mut {rt}::types::private::BytesMut) -> Result<{rt}::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {{
        {rt}::enum_to_sql(self, out)
    }}
    fn accepts(ty: &{rt}::Type) -> bool {{
        ty.name() == {typname:?}
    }}
    {rt}::types::to_sql_checked!();
}}
"
    );
    Ok(o)
}

fn qname(qualified: &str) -> Ty {
    pgweave_query::descriptor::parse_qualified(qualified)
        .unwrap_or_else(|| pgweave_sema::pg(qualified))
}

/// Mots d'un nom, en minuscules : séparés par tout caractère non
/// alphanumérique et par les changements de casse (`createdAt` → created,
/// at ; `HTTPServer` → http, server ; `ORDERED` → ordered).
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in s
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
    {
        let chars: Vec<char> = part.chars().collect();
        let mut word = String::new();
        for (i, &c) in chars.iter().enumerate() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + 1);
            let boundary = c.is_ascii_uppercase()
                && prev.is_some_and(|p| {
                    p.is_ascii_lowercase()
                        || p.is_ascii_digit()
                        || (p.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()))
                });
            if boundary && !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            word.push(c.to_ascii_lowercase());
        }
        if !word.is_empty() {
            out.push(word);
        }
    }
    out
}

/// `page_by_id` → `PageById`, `ORDERED` → `Ordered`.
fn pascal(s: &str) -> String {
    words(s)
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "abstract", "become", "box", "do", "final", "gen", "macro", "override",
    "priv", "try", "typeof", "unsized", "virtual", "yield",
];

/// Identifiant Rust d'un nom de requête (tel quel, il doit en être un).
fn rust_ident(name: &str) -> Result<String, String> {
    field_ident(name)
}

/// Champ Rust d'un nom de colonne ou de paramètre : en snake_case
/// (`createdAt` → `created_at`, `ID` → `id`), mot-clé en identifiant brut.
fn field_ident(name: &str) -> Result<String, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("`{name}` n'est pas un identifiant Rust"));
    }
    let out = words(name).join("_");
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(format!("`{name}` n'est pas un identifiant Rust"));
    }
    Ok(if KEYWORDS.contains(&out.as_str()) {
        format!("r#{out}")
    } else {
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noms_rust() {
        for (name, field, ty) in [
            ("createdAt", "created_at", "CreatedAt"),
            ("companyId", "company_id", "CompanyId"),
            ("ID", "id", "Id"),
            ("HTTPServer", "http_server", "HttpServer"),
            ("ORDERED", "ordered", "Ordered"),
            ("IN_PROGRESS", "in_progress", "InProgress"),
            ("page_by_id", "page_by_id", "PageById"),
            ("v2Count", "v2_count", "V2Count"),
            ("type", "r#type", "Type"),
        ] {
            assert_eq!(field_ident(name).unwrap(), field, "{name}");
            assert_eq!(pascal(name), ty, "{name}");
        }
        assert!(field_ident("?column?").is_err());
    }
}
