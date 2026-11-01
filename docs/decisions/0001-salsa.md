# 0001 — salsa comme moteur incrémental du catalogue

- **Statut** : acceptée (GO), 2026-11-01
- **Contexte** : PLAN.md, décision D3 et item P0 « spike salsa »
- **Preuve** : `spikes/salsa-catalog` (jetable, supprimé en P2 — voir l'historique git)

## Question

Le catalogue (D2) doit être : indexé à moindre coût, résolu **à la demande**
(seuls les objets référencés sont analysés), organisé en **portées** (projet,
routine, bloc) et **incrémental** pour le LSP. salsa — utilisé par
rust-analyzer et `ty` — convient-il, ou faut-il un cache maison ?

Critères d'abandon fixés à l'avance (D3) : impossibilité d'exprimer la
portée « routine » proprement, ou surcoût d'indexation > 2× sur Mesalia.

## Ce que le spike construit

```text
SourceFile (input) ──► file_declarations ──► table_index ─┐
       │               (scan de tokens,       (nom → fichiers,
       │                pas de parse)          dans l'ordre)
       └──────────► parse_file ◄──────────────────────────┴─ resolve_table(nom)
                    (parse complet,                         (DDL appliqués dans
                     seulement si demandé)                   l'ordre des fichiers)

routine_scope(nom) : paramètres, variables, tables temporaires du corps
resolve_in_routine(routine, nom) : table temporaire d'abord, puis globale
```

La sémantique d'application des DDL reproduit celle du `SchemaResolver`
actuel, pour comparer les deux à l'identique.

## Résultats

### Propriétés (tests `spikes/salsa-catalog/tests/spike.rs`)

| Propriété | Résultat |
|---|---|
| L'index se construit **sans parser** aucun fichier | ✅ |
| Résoudre une table ne parse **que** les fichiers qui la touchent, et applique `CREATE` puis `ALTER` dans l'ordre | ✅ |
| Modifier un fichier sans changer ses déclarations : seules ses déclarations sont relues, **rien d'autre n'est recalculé** (l'index et les tables sont réutilisés) | ✅ |
| Ajouter une colonne ne recalcule que la table concernée | ✅ |
| Portée de routine : paramètres, variables `DECLARE`, tables temporaires du corps ; une table temporaire masque la table globale et n'existe pas hors de la routine | ✅ |

### Mesalia (220 migrations, build `--release`)

| Mesure | Valeur |
|---|--:|
| Tables | 134 |
| Équivalence avec `SchemaResolver` (colonnes, types, nullabilité) | **134/134 identiques** |
| `SchemaResolver::from_chunks` actuel (eager) | 51,5 ms |
| salsa : index seul | 4,3 ms |
| salsa : index + résolution de **toutes** les tables | 23,4 ms |
| salsa : après modification d'un fichier, résolution de toutes les tables | 0,07 ms |
| Requêtes ré-exécutées après cette modification | 3 (déclarations et parse du fichier, la seule table qu'il touche) |

Le critère « surcoût < 2× » est largement tenu : salsa est **plus rapide**
que l'existant même en résolvant tout (l'existant fait aussi le
désucrage des macros et un second index ; la comparaison reste
favorable à périmètre comparable), et une modification coûte des
microsecondes.

## Décision

**GO.** `pgweave-project` (P2) est construit sur salsa, version **épinglée**
(`=0.28.5`), montée de version délibérée et isolée.

## Ce que le spike a appris, à reprendre en P2

1. **L'AST n'implémente pas `Eq`** : `parse_file` est donc `no_eq`, et tout
   changement de texte ré-exécute les requêtes qui lisent ce parse (elles
   s'arrêtent ensuite si leur résultat est identique). Pour une granularité
   plus fine : AST comparable, ou parse **par statement** (clé = texte du
   statement), de sorte qu'éditer une fonction ne relise pas les autres
   statements du fichier.
2. **Le scan de déclarations duplique une connaissance du parser** (forme
   des têtes de statements). Garde-fou : un test d'équivalence systématique
   scan ↔ parse complet, comme celui fait ici sur Mesalia. Le scan devra
   couvrir tout ce que P1 modélise (types, vues, extensions, `DROP`…).
3. **API salsa mouvante** : le trait `Update` est devenu `SalsaValue` entre
   versions. D'où l'épinglage exact et l'isolement de salsa dans
   `pgweave-project` : les autres crates manipulent des types à nous.
4. **Pas encore évalués** : requêtes parallèles et annulation (le LSP
   annule une analyse obsolète), persistance sur disque (fonctionnalité
   `persistence`, utile pour le cache des macros Rust en P5). À valider au
   démarrage de P2 et de P5.
5. **Casse des identifiants** : le spike reproduit la mise en minuscules du
   code actuel pour comparer ; le catalogue de P2 appliquera le principe 8
   (identifiants quotés conservés).
