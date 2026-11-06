# pgzest JetBrains Plugin

LSP-based PostgreSQL support for IntelliJ IDEA, DataGrip, and other JetBrains IDEs.

## Requirements

- IntelliJ IDEA 2024.1+ (Community or Ultimate)
- [LSP4IJ plugin](https://plugins.jetbrains.com/plugin/23257-lsp4ij) installed
- the `pgzest` binary on your PATH (`cargo install pgzest`); the plugin runs `pgzest lsp`

## Installation (development)

```bash
cd editors/jetbrains
./gradlew runIde   # Launches a sandbox IDE with the plugin loaded
```

## Building

```bash
./gradlew buildPlugin  # Produces build/distributions/pgzest-0.1.0.zip
```

## Configuration

Set the `PGZEST_PATH` environment variable to point to the `pgzest` binary
if it's not on your PATH:

```bash
export PGZEST_PATH=/path/to/pgzest
```
