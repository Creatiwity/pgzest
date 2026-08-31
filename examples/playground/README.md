# Playground pgsql-tools

Projet de démonstration. Ouvert automatiquement par **F5** depuis
`editors/vscode/`.

## Lancer

```bash
make lsp                 # compile le serveur LSP (target/debug/pgsql-lsp)
code editors/vscode      # puis F5 → « Run Extension (playground) »
```

L'extension détecte `target/debug/pgsql-lsp` toute seule. Une fenêtre
« Extension Development Host » s'ouvre sur ce dossier.

En CLI, depuis ce dossier :

```bash
pgsql check queries/02-erreurs.sql
pgsql lint  queries/05-lint.sql
```

## Ce qu'il y a à regarder

| Fichier | À observer |
|---|---|
| `queries/01-ok.sql` | Entièrement propre. `users` et `organizations` viennent de `schema/tables/`, indexé via la config — aucun `--schema` à passer. |
| `queries/02-erreurs.sql` | Colonne et table inexistantes. Le hint liste les colonnes disponibles. |
| `queries/03-directives.sql` | Les directives en commentaire, à la ESLint. Seul le dernier `SELECT` doit être souligné. |
| `queries/04-tolerance-parser.sql` | Un statement illisible remonte en warning sans interrompre l'analyse du reste. |
| `queries/05-lint.sql` | Règles de lint. **Invisibles avec `pgsql check`** : le linter est une commande séparée. Visibles dans VS Code, où le LSP lance les deux. |

Essaie aussi :

- **Hover** sur une colonne dans `01-ok.sql` → son type.
- **Complétion** après `SELECT ` ou après `u.` dans une requête sur `users`.
- Modifier `schema/tables/users.sql` (ajouter une colonne) puis revenir sur
  `queries/01-ok.sql` : l'index se reconstruit.
- Passer `unparsable = "error"` dans `pgsql-tools.config.toml` : le warning de
  `04-tolerance-parser.sql` devient une erreur.

## Structure

```
pgsql-tools.config.toml   ← définit le scope, la tolérance, les sévérités
schema/
  tables/                 ← indexé récursivement…
  functions/              ← …sous-répertoires compris
migrations/
  000001_init.sql         ← indexée avec sa séquence
queries/                  ← hors scope schema : rôle Query, DDL interdit
```

`schema/` est canonique : les migrations en sont dérivées. Les deux doivent
rester cohérents — ici `last_seen_at` figure dans les deux.

## Limites que tu vas voir

- Les diagnostics du **type-checker** visent l'expression fautive (`nope` est
  souligné, pas tout le `SELECT`). Les règles de **lint** visent le statement
  entier — leurs nœuds d'intérêt ne sont pas toujours des expressions.
- Une expression synthétisée par l'outil n'a pas de position : le `true` issu
  du désucrage de `IS TRUE`, par exemple. Un diagnostic dessus retombe sur le
  statement.
- `table-not-in-scope` (référence à une table absente du `FROM`) ne se
  déclenche pas encore au niveau top-level.
- Une même fonction trigger servant plusieurs tables produit un faux positif
  `trigger-ambiguous-table`.
