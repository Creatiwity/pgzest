# pgweave JetBrains Plugin

LSP-based PostgreSQL support for IntelliJ IDEA, DataGrip, and other JetBrains IDEs.

## Requirements

- IntelliJ IDEA 2024.1+ (Community or Ultimate)
- [LSP4IJ plugin](https://plugins.jetbrains.com/plugin/23257-lsp4ij) installed
- `pgweave-lsp` binary on your PATH (build from source: `cargo build --bin pgweave-lsp --release`)

## Installation (development)

```bash
cd editors/jetbrains
./gradlew runIde   # Launches a sandbox IDE with the plugin loaded
```

## Building

```bash
./gradlew buildPlugin  # Produces build/distributions/pgweave-0.1.0.zip
```

## Configuration

Set `PGWEAVE_LSP_PATH` environment variable to point to the pgweave-lsp binary
if it's not on your PATH:

```bash
export PGWEAVE_LSP_PATH=/path/to/pgweave-lsp
```
