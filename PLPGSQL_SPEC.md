# pgsql-tools — Spec : Support complet du dialecte PostgreSQL / PL/pgSQL

## Situation actuelle

La couche parser repose sur **sqlparser-rs v0.54** avec `PostgreSqlDialect`. Ce dialecte couvre la
quasi-totalité du SQL standard PostgreSQL (DDL, DML, policies, triggers) mais laisse deux trous :

1. **Corps de fonctions/procédures PL/pgSQL** (`$$...$$`) — le corps est tokenisé comme une
   `DollarQuotedString` opaque et stocké tel quel dans `FunctionBody::Raw(String)`. Aucun AST
   structuré n'est produit.

2. **Options de fonction manquantes** dans `parse_postgres_create_function` :
   - `SECURITY DEFINER` / `SECURITY INVOKER`
   - `COST n` / `ROWS n`
   - `SET config_param TO value` (options de configuration)
   - `RETURNS SETOF type` (partiellement géré, mais pas toujours)

---

## Architecture de la solution

### Ce que sqlparser-rs fournit comme points d'extension

La documentation officielle recommande le pattern **"wrapper dialect"** :

> *"Write a new parser that delegates to the ANSI parser so that as much as possible of the core
> functionality can be re-used."*

Le trait `Dialect` expose quatre hooks clés :

```rust
// Appelé en premier à chaque `parse_statement()`. Retourner Some(...) court-circuite le défaut.
fn parse_statement(&self, parser: &mut Parser) -> Option<Result<Statement, ParserError>>

// Appelé en premier à chaque parse d'expression préfixe.
fn parse_prefix(&self, parser: &mut Parser) -> Option<Result<Expr, ParserError>>

// Appelé en premier à chaque expression infixe.
fn parse_infix(&self, parser: &mut Parser, expr: &Expr, prec: u8)
    -> Option<Result<Expr, ParserError>>

// Appelé pour chaque option de colonne dans CREATE TABLE.
fn parse_column_option(&self, parser: &mut Parser)
    -> Result<Option<Result<Option<ColumnOption>, ParserError>>, ParserError>
```

**Ce qui est déjà fait dans sqlparser-rs pour PostgreSQL :**
- `Token::DollarQuotedString` — le tokenizer extrait déjà le corps `$$...$$` en string
- `parse_create_function_body_string()` — produit `Expr::Value(Value::DollarQuotedString(s))`
- `CreatePolicy`, `CreateTrigger`, `AlterTable { EnableRowLevelSecurity }` — tous parsés

**Ce qui manque :**
- `SECURITY DEFINER` / `SECURITY INVOKER` dans le loop d'options de `parse_postgres_create_function`
- Parser structurel du corps PL/pgSQL lui-même

---

## Plan d'implémentation

### Partie A — Dialect wrapper (sans fork)

Créer dans `crates/pgsql-parser/src/dialect.rs` un `PgsqlToolsDialect` qui enveloppe
`PostgreSqlDialect` et surcharge `parse_statement()` pour intercepter `CREATE FUNCTION` /
`CREATE PROCEDURE` et les repasser à notre propre logique d'extraction des options manquantes.

```rust
pub struct PgsqlToolsDialect;

impl Dialect for PgsqlToolsDialect {
    fn parse_statement(&self, parser: &mut Parser)
        -> Option<Result<Statement, ParserError>>
    {
        // On laisse sqlparser parser le statement normalement...
        None
        // ...puis on post-traite pour injecter SECURITY DEFINER etc.
        // (voir Partie A2 : post-processing)
    }

    // Délégation intégrale pour le reste
    fn is_identifier_start(&self, ch: char) -> bool { ... }
    fn is_identifier_part(&self, ch: char) -> bool { ... }
    // + toutes les méthodes supports_*() de PostgreSqlDialect
}
```

**Pourquoi ne pas forker ?**
- sqlparser-rs est activement maintenu (releases fréquentes). Un fork crée une dette de merge
  permanente pour des centaines de méthodes `supports_*`.
- Le hook `parse_statement()` est suffisant pour intercepter ce dont on a besoin.
- Les `DollarQuotedString` sont déjà extraites — on n'a pas besoin de toucher au tokenizer.

#### A1 — Options manquantes dans CREATE FUNCTION

Le problème : après que sqlparser ait parsé le `Statement::CreateFunction`, le champ
`security_definer` est absent du struct sqlparser (il n'existe pas dans la codebase v0.54).

**Solution :** post-processing dans `convert.rs`. Au moment de convertir `CreateFunction`,
re-scanner le SQL source (ou les tokens résiduels) pour détecter `SECURITY DEFINER`.

Plus proprement : dans le hook `parse_statement`, intercepter `CREATE [OR REPLACE] FUNCTION`,
déléguer à sqlparser pour tout sauf les mots-clés non gérés, puis absorber nous-mêmes
`SECURITY DEFINER | SECURITY INVOKER | COST n | ROWS n | SET param TO value`.

```
CREATE OR REPLACE FUNCTION foo() RETURNS TRIGGER
  LANGUAGE plpgsql
  SECURITY DEFINER          ← sqlparser ignore ce token → panic ou erreur
  AS $$ ... $$;
```

Actuellement sqlparser s'arrête avant `SECURITY DEFINER` et laisse des tokens non consommés
→ erreur de parsing. Notre hook doit consommer ces tokens.

#### A2 — `RETURNS SETOF type`

`RETURNS SETOF uuid` est partiellement géré via `DataType::Custom("SETOF")` mais sans le type
interne. Notre `convert_return_type()` doit détecter ce cas et produire `ReturnType::SetOf(inner)`.

---

### Partie B — Parser PL/pgSQL

C'est la pièce centrale. Le corps `$$...$$` est une string — il faut un **mini-parser Rust**
pour le décomposer en AST structuré.

#### B1 — AST PL/pgSQL (nouveau fichier `crates/pgsql-ast/src/plpgsql.rs`)

```rust
/// Corps complet d'une fonction ou procédure PL/pgSQL
pub struct PlpgsqlBody {
    pub declare: Vec<VarDecl>,
    pub block: Block,
}

/// Déclaration de variable dans la section DECLARE
pub struct VarDecl {
    pub name: Ident,
    pub data_type: DataType,
    pub not_null: bool,
    pub default: Option<PlExpr>,
    pub constant: bool,
}

/// Bloc BEGIN ... END (potentiellement labellisé)
pub struct Block {
    pub label: Option<String>,
    pub statements: Vec<PlStatement>,
    pub exception: Option<ExceptionBlock>,
}

pub struct ExceptionBlock {
    pub handlers: Vec<ExceptionHandler>,
}

pub struct ExceptionHandler {
    pub conditions: Vec<String>,   // SQLSTATE ou nom de condition
    pub block: Block,
}

/// Statement PL/pgSQL
pub enum PlStatement {
    // Assignation : var := expr
    Assign { target: AssignTarget, value: PlExpr },
    // SELECT INTO : SELECT ... INTO [STRICT] var FROM ...
    SelectInto { strict: bool, target: Vec<Ident>, query: SelectStmt },
    // PERFORM : exécute une requête et jette le résultat
    Perform(PlExpr),
    // EXECUTE dynamique
    Execute { sql: PlExpr, into: Option<Vec<Ident>>, using: Vec<PlExpr> },
    // RETURN / RETURN NEXT / RETURN QUERY
    Return(Option<PlExpr>),
    ReturnNext(PlExpr),
    ReturnQuery(SelectStmt),
    // RAISE
    Raise { level: RaiseLevel, message: RaiseMessage, options: Vec<RaiseOption> },
    // IF / ELSIF / ELSE
    If { condition: PlExpr, then: Block, elsif: Vec<(PlExpr, Block)>, else_: Option<Block> },
    // Boucles
    Loop(Block),
    While { condition: PlExpr, body: Block },
    ForIn { var: Ident, from: PlExpr, to: PlExpr, by: Option<PlExpr>, body: Block, reverse: bool },
    ForSelect { var: Ident, query: SelectStmt, body: Block },
    ForExecute { var: Ident, sql: PlExpr, body: Block },
    // Contrôle de flux
    Exit { label: Option<String>, when: Option<PlExpr> },
    Continue { label: Option<String>, when: Option<PlExpr> },
    // DML embarqué
    Sql(Statement),
    // CALL procédure
    Call { name: ObjectName, args: Vec<PlExpr> },
    // Bloc imbriqué
    SubBlock(Block),
    // GET DIAGNOSTICS
    GetDiagnostics(Vec<(Ident, DiagnosticsItem)>),
    // NULL (no-op)
    Null,
}

pub enum AssignTarget {
    Simple(Ident),
    RowField(Ident, Ident),   // row.field
}

pub enum RaiseLevel { Debug, Log, Info, Notice, Warning, Exception }

pub enum RaiseMessage {
    Format { format: String, args: Vec<PlExpr> },
    Using,   // RAISE USING option = ...
}

pub struct RaiseOption { pub key: RaiseOptionKey, pub value: PlExpr }
pub enum RaiseOptionKey { Message, Detail, Hint, Errcode, Column, Constraint, Datatype, Table, Schema }

pub enum DiagnosticsItem {
    RowCount, PgContext, PgExceptionContext, PgExceptionDetail, PgExceptionHint,
    ReturnedSqlstate, ColumnName, ConstraintName, PgDatatype, MessageText, TableName, SchemaName,
}

/// Expression PL/pgSQL (superset de Expr SQL)
pub enum PlExpr {
    Sql(Expr),                 // expression SQL standard
    Var(Ident),                // variable locale
    RowField(Ident, Ident),    // row.field
    Found,                     // variable spéciale FOUND
    NullExpr,                  // NULL
}
```

#### B2 — Parser PL/pgSQL (`crates/pgsql-parser/src/plpgsql/mod.rs`)

Écrire un parser récursif descendant qui opère sur une string (le corps `$$...$$`).

**Approche** : tokenizer minimal dédié au PL/pgSQL + parser à la main.
Ne pas réutiliser le tokenizer sqlparser pour le corps — les mots-clés PL/pgSQL comme
`ELSIF`, `LOOP`, `RAISE`, `PERFORM` ne sont pas des keywords SQL standard, et la gestion des
`DECLARE` / variables locales est orthogonale au SQL.

Structure :

```
crates/pgsql-parser/src/plpgsql/
  mod.rs         → pub fn parse_plpgsql_body(body: &str) -> Result<PlpgsqlBody, PlParseError>
  lexer.rs       → Tokenizer PL/pgSQL minimal (identifiers, keywords, strings, numbers, ops)
  parser.rs      → Parser récursif descendant
  keywords.rs    → Enum des keywords PL/pgSQL
  error.rs       → PlParseError avec span
```

**Tokenizer** : réutiliser `sqlparser::tokenizer::Tokenizer` pour les expressions SQL (SELECT,
INSERT...) à l'intérieur du corps — on n'a besoin du tokenizer custom que pour la structure
de contrôle PL/pgSQL elle-même.

**Intégration dans convert.rs** :

```rust
// Actuellement :
FunctionBody::Raw(body_string)

// Après :
match pgsql_parser::plpgsql::parse_plpgsql_body(&body_string) {
    Ok(parsed) => FunctionBody::Parsed(Box::new(parsed)),
    Err(e) => {
        // Dégradation gracieuse : on garde le Raw avec un diagnostic
        FunctionBody::Raw(body_string)
    }
}
```

`FunctionBody` devient :
```rust
pub enum FunctionBody {
    Raw(String),                         // pas encore parsé ou parse échoué
    SqlQuery(Box<SelectStmt>),           // LANGUAGE sql simple
    Parsed(Box<plpgsql::PlpgsqlBody>),   // LANGUAGE plpgsql parsé
}
```

---

### Partie C — Checker PL/pgSQL structurel

Une fois l'AST PL/pgSQL disponible, `pgsql-checker` peut faire du vrai type-checking :

```rust
pub fn check_plpgsql_body(
    body: &PlpgsqlBody,
    function_sig: &CreateFunction,
    schema: &dyn SchemaProvider,
) -> Vec<Diagnostic>
```

**Ce que le checker peut valider :**

| Construct | Validation |
|---|---|
| `DECLARE v type` | Type reconnu dans SchemaProvider ou type PG built-in |
| `v := expr` | Type de `expr` compatible avec le type déclaré de `v` |
| `SELECT INTO v FROM t WHERE ...` | Colonnes existent sur `t`, type de `v` compatible |
| `OLD.col` / `NEW.col` | Colonne existe sur la table du trigger |
| `RAISE EXCEPTION msg` | Toujours valide |
| `EXECUTE sql_string` | Warn W004 (SQL dynamique non analysable) |
| Appel de fonction `f(args)` | Signature connue via SchemaProvider.get_function() |
| `RETURN expr` | Type de `expr` compatible avec `RETURNS` déclaré |
| Variable non déclarée | E002 "variable 'x' not declared" |
| Variable utilisée avant assignation | W005 (best-effort) |

**Contexte de type** : un `TypeContext` est maintenu pendant le parcours du corps :

```rust
struct PlTypeContext<'a> {
    variables: HashMap<String, DataType>,   // DECLARE vars
    params: HashMap<String, DataType>,      // paramètres de la fonction
    return_type: &'a ReturnType,
    trigger_table: Option<&'a TableSchema>, // pour OLD/NEW
    schema: &'a dyn SchemaProvider,
}
```

---

## Roadmap de mise en oeuvre

### Étape 1 — Dialect wrapper + options manquantes *(~1 jour)*

**Fichiers à créer/modifier :**
- `crates/pgsql-parser/src/dialect.rs` — `PgsqlToolsDialect`
- `crates/pgsql-parser/src/lib.rs` — utiliser `PgsqlToolsDialect` au lieu de `PostgreSqlDialect`
- `crates/pgsql-parser/src/convert.rs` — post-processing `SECURITY DEFINER`, `COST`, `ROWS`, `RETURNS SETOF`
- `crates/pgsql-ast/src/ddl.rs` — ajouter `security_definer`, `cost`, `rows` sur `CreateFunction`

**Test de validation :**
```sql
CREATE OR REPLACE FUNCTION current_user_org_ids()
RETURNS SETOF UUID
LANGUAGE sql STABLE SECURITY DEFINER AS $$ ... $$;
```
→ Parse sans erreur, `security_definer = true`, `return_type = ReturnType::SetOf(DataType::Uuid)`

---

### Étape 2 — AST PL/pgSQL *(~0.5 jour)*

**Fichiers :**
- `crates/pgsql-ast/src/plpgsql.rs` — tous les types de l'AST PL/pgSQL ci-dessus
- `crates/pgsql-ast/src/lib.rs` — `pub mod plpgsql`

---

### Étape 3 — Parser PL/pgSQL *(~3-4 jours — c'est le gros morceau)*

**Fichiers :**
- `crates/pgsql-parser/src/plpgsql/mod.rs`
- `crates/pgsql-parser/src/plpgsql/lexer.rs`
- `crates/pgsql-parser/src/plpgsql/parser.rs`
- `crates/pgsql-parser/src/plpgsql/keywords.rs`
- `crates/pgsql-parser/src/plpgsql/error.rs`

**Ordre d'implémentation recommandé** (de simple à complexe) :
1. Lexer + keywords + tests unitaires du lexer
2. Parser de bloc `BEGIN...END` avec statements `NULL` et `RETURN`
3. `RAISE`
4. `IF/ELSIF/ELSE/END IF`
5. `ASSIGN (:=)` + `DECLARE`
6. `SELECT INTO`, `PERFORM`
7. Boucles (`LOOP`, `WHILE`, `FOR`)
8. `EXECUTE` dynamique
9. `EXCEPTION` handlers
10. Blocs imbriqués, labels, `EXIT`/`CONTINUE`

**Test de validation principal :**
Parmi les fonctions de `examples/schema.sql` :
- `set_updated_at()` — trivial (BEGIN, NEW.updated_at := now(), RETURN NEW)
- `enforce_task_status_transition()` — IF/ELSIF, NEW.col, RAISE EXCEPTION
- `assign_task()` — DECLARE, SELECT INTO, IF NOT FOUND, UPDATE

---

### Étape 4 — Checker PL/pgSQL structurel *(~2 jours)*

**Fichiers :**
- `crates/pgsql-checker/src/check/plpgsql.rs` — remplace le scanner actuel
- `crates/pgsql-checker/src/check/type_context.rs` — `PlTypeContext`

**Test de validation :**
```sql
-- Doit produire E001 "column 'nonexistent' does not exist on table 'tasks'"
CREATE OR REPLACE FUNCTION bad_trigger() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  NEW.nonexistent := now();
  RETURN NEW;
END;
$$;
```

---

### Étape 5 — Intégration end-to-end *(~1 jour)*

- `examples/schema.sql` parsé à 100% sans `FunctionBody::Raw` résiduel
- Tous les checks sur les corps PL/pgSQL de l'exemple passent
- Mise à jour du test `parse_full_schema_counts_statements` :
  tous les statements → `ok`, 0 `syntax` ou `unsupported`

---

## Résumé des gaps et leur solution

| Gap | Solution | Complexité |
|---|---|---|
| `SECURITY DEFINER` non parsé | Dialect wrapper + post-processing | Faible |
| `RETURNS SETOF type` incomplet | Post-processing dans convert.rs | Faible |
| `COST n` / `ROWS n` ignorés | Idem | Faible |
| Corps PL/pgSQL opaque | Mini-parser dédié (Étape 3) | **Élevée** |
| Type-check des corps PL/pgSQL | Checker structurel (Étape 4) | Élevée |
| Variables non déclarées | Couvert par Étape 4 | — |
| `EXECUTE` (SQL dynamique) | Warning W004, pas d'analyse | Faible |
| Extensions custom (PostGIS…) | Plugin `dialect.rs` existant | Faible |

---

## Ce qu'on ne parsera pas (hors scope justifié)

| Construct | Raison |
|---|---|
| `EXECUTE format(...)` avec SQL dynamique | Impossible statiquement par nature |
| `COPY` (streaming) | Pas de cas d'usage dans le toolchain |
| `LISTEN` / `NOTIFY` DDL | Parsé par sqlparser, pas de type-check nécessaire |
| Corps C / internal | Binaires opaques, rien à analyser |
| PL/Python, PL/Perl, etc. | Langages externes, hors périmètre |

---

## Dépendances à ajouter

Aucune dépendance externe supplémentaire n'est nécessaire — le mini-parser PL/pgSQL est
écrit from scratch en Rust pur, ce qui garantit la portabilité WASM et l'absence de
dépendances système.
