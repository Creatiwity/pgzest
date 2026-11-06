# Releasing pgzest

A release is a version tag pushed to `main`. The
[`Release`](.github/workflows/release.yml) workflow then builds the
`pgzest` binary and the VS Code extension for each platform, creates the
GitHub release, and publishes to crates.io, the VS Code Marketplace and
Open VSX. Those three publishing jobs wait for an approval in the
`release` environment.

| What | Where |
|------|-------|
| Crates (`pgzest`, `pgzest-runtime`, `pgzest-build`, `pgzest-macros` and the crates they depend on) | crates.io |
| `pgzest` binary: macOS arm64, Linux x64 and arm64 (glibc ≥ 2.35), Windows x64 | GitHub release (archives + SHA-256) |
| VS Code extension, one package per platform with the binary inside | GitHub release, VS Code Marketplace, Open VSX |

## Commit messages

From 0.1.0 on, commits follow
[Conventional Commits](https://www.conventionalcommits.org): `feat:`,
`fix:`, `perf:`, `refactor:`, `docs:`, `chore:`, `ci:`, `build:`,
`test:`, with an optional scope (`fix(lsp): …`) and `!` or a
`BREAKING CHANGE:` footer for a breaking change. `CHANGELOG.md` is
generated from them by [git-cliff](https://git-cliff.org)
([`cliff.toml`](./cliff.toml)): commits that do not follow the
convention do not appear in it.

## Making a release

1. Pick the version (Semantic Versioning; before 1.0, a breaking change
   bumps the minor version) and set it in two places:
   - `Cargo.toml`: `[workspace.package] version`, and the `version` of
     the internal crates in `[workspace.dependencies]`;
   - `editors/vscode/package.json`: `version`.
2. Generate the changelog section (for 0.1.0, written by hand: replace
   `Unreleased` with the date):

   ```sh
   git cliff --unreleased --tag vX.Y.Z --prepend CHANGELOG.md
   ```

   Review it, then check that everything publishes:

   ```sh
   cargo publish --workspace --dry-run --locked
   (cd editors/vscode && npm run compile && npx vsce package)
   ```

3. Commit and tag:

   ```sh
   git commit -am "chore(release): vX.Y.Z"
   git tag vX.Y.Z
   git push origin main vX.Y.Z
   ```

4. The workflow checks that the tag, the crates, the extension and the
   changelog agree, builds, and creates the GitHub release. Approve the
   `release` environment in the workflow run to publish.

A failed publishing job can be re-run from the workflow; `cargo publish`
refuses a version already on crates.io, so after a partial crates.io
publication, publish the remaining crates by hand (`cargo publish -p …`)
in dependency order.

## One-time setup

### crates.io

1. Sign in at [crates.io](https://crates.io) with the GitHub account that
   will own the crates.
2. [Account settings → API Tokens](https://crates.io/settings/tokens):
   a token with the `publish-new` and `publish-update` scopes.
3. Repository secret `CARGO_REGISTRY_TOKEN`.

After the first publication, add co-owners if needed
(`cargo owner --add github:Creatiwity:<team> <crate>`).

### VS Code Marketplace

The extension's identifier is `<publisher>.pgzest`; the publisher is
`creatiwity` (`publisher` in `editors/vscode/package.json`).

1. Sign in to [Azure DevOps](https://dev.azure.com) with the Microsoft
   account that will own the publisher (create an organization if asked:
   it is only used for the token).
2. Create the publisher on the
   [Marketplace management page](https://marketplace.visualstudio.com/manage/createpublisher):
   ID `creatiwity`, name Creatiwity.
3. In Azure DevOps, *User settings → Personal access tokens → New
   token*: organization **All accessible organizations**, scope
   **Marketplace → Manage**, the longest expiry allowed (one year; note
   the renewal).
4. Repository secret `VSCE_PAT`.

### Open VSX (VSCodium, Cursor, Windsurf, Gitpod…)

1. Sign in at [open-vsx.org](https://open-vsx.org) with GitHub, and sign
   the Eclipse Foundation publisher agreement it asks for (an Eclipse
   account linked to the GitHub one).
2. [Settings → Access Tokens](https://open-vsx.org/user-settings/tokens):
   generate a token.
3. Create the namespace once, matching the publisher:

   ```sh
   npx ovsx create-namespace creatiwity -p <token>
   ```

   then claim its ownership (verified namespace) through the
   [namespace claim](https://github.com/EclipseFdn/open-vsx.org/issues/new/choose)
   issue template.
4. Repository secret `OVSX_PAT`.

### The `release` environment

*Settings → Environments → New environment* `release`, with
**Required reviewers** (the maintainers) and, under *Deployment branches
and tags*, only the tags `v*`. The three secrets above can be stored in
this environment rather than in the repository.
