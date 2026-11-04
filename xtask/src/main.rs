//! pgweave development tasks: `cargo xtask <command>`.

mod catalog_compare;
mod catalog_gen;
mod corpus;
mod docs;
mod migrate_oracle;
mod probes;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo xtask", about = "pgweave development tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// "Analysis rate" report on an external migration corpus.
    ///
    /// The corpus is never copied into the repository: its path comes from
    /// `--path` or from the `PGWEAVE_CORPUS_<NAME>` variable (e.g.
    /// `PGWEAVE_CORPUS_MESALIA`). The report only contains aggregates and
    /// statement shapes with names masked.
    Corpus {
        /// Corpus name (used for the title and the environment variable).
        name: String,
        /// Corpus directory.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Layout of the migrations in the directory.
        #[arg(long, value_enum, default_value = "flat")]
        layout: corpus::Layout,
        /// Test PostgreSQL server on which to replay the migrations
        /// (default: `PGWEAVE_ORACLE_URL`). A database is created then dropped there.
        #[arg(long)]
        pg_url: Option<String>,
        /// Markdown file to write (default: standard output).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Prints the detail of each gap on standard error, names included.
        /// For local work only: never written to the report.
        #[arg(long)]
        details: bool,
    },
    /// Generates a catalog pack from a real PostgreSQL (PLAN.md, D9).
    ///
    /// Without `--extension`: `postgres@<major>`. With it: the delta brought by
    /// `CREATE EXTENSION`. A temporary database is created then dropped.
    CatalogGen {
        /// PostgreSQL server (default: `PGWEAVE_ORACLE_URL`).
        #[arg(long)]
        pg_url: Option<String>,
        /// Extension whose pack to generate.
        #[arg(long)]
        extension: Option<String>,
        /// Extension version (default: the server's default one).
        #[arg(long)]
        version: Option<String>,
        /// File to write (default: `crates/pgweave-catalog/packs/<name>-<version>.json`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Migration oracle of a declarative project: applied migrations and the
    /// declarative schema must yield two identical databases (P7).
    MigrateOracle {
        /// Project directory (`pgweave.toml`).
        project: PathBuf,
        /// PostgreSQL server (default: `PGWEAVE_ORACLE_URL`).
        #[arg(long)]
        pg_url: Option<String>,
        /// Prints the gaps, names included (standard error, local work).
        #[arg(long)]
        details: bool,
    },
    /// Generates the reference pages of the documentation site from the
    /// code: CLI (`--help`) and diagnostics (documented code constants).
    Docs {
        /// Fails if a page is stale or a code is not a documented constant
        /// (CI).
        #[arg(long)]
        check: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Corpus {
            name,
            path,
            layout,
            pg_url,
            out,
            details,
        } => corpus::run(&name, path, layout, pg_url, out, details),
        Command::CatalogGen {
            pg_url,
            extension,
            version,
            out,
        } => catalog_gen::run(pg_url, extension, version, out),
        Command::MigrateOracle {
            project,
            pg_url,
            details,
        } => migrate_oracle::run(&project, pg_url, details),
        Command::Docs { check } => docs::run(check),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
