# pgsql-tools JetBrains Plugin

LSP-based PostgreSQL support for IntelliJ IDEA, DataGrip, and other JetBrains IDEs.

## Requirements

- IntelliJ IDEA 2024.1+ (Community or Ultimate)
- [LSP4IJ plugin](https://plugins.jetbrains.com/plugin/23257-lsp4ij) installed
- `pgsql-lsp` binary on your PATH (build from source: `cargo build --bin pgsql-lsp --release`)

## Installation (development)

```bash
cd editors/jetbrains
./gradlew runIde   # Launches a sandbox IDE with the plugin loaded
```

## Building

```bash
./gradlew buildPlugin  # Produces build/distributions/pgsql-tools-0.1.0.zip
```

## Configuration

Set `PGSQL_LSP_PATH` environment variable to point to the pgsql-lsp binary
if it's not on your PATH:

```bash
export PGSQL_LSP_PATH=/path/to/pgsql-lsp
```
