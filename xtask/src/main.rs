//! Tâches de développement de pgweave : `cargo xtask <commande>`.

mod corpus;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo xtask", about = "Tâches de développement de pgweave")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Rapport « taux d'analyse » sur un corpus externe de migrations.
    ///
    /// Le corpus n'est jamais copié dans le dépôt : son chemin vient de
    /// `--path` ou de la variable `PGWEAVE_CORPUS_<NOM>` (ex.
    /// `PGWEAVE_CORPUS_MESALIA`). Le rapport ne contient que des agrégats et
    /// des formes de statements dont les noms sont masqués.
    Corpus {
        /// Nom du corpus (sert au titre et à la variable d'environnement).
        name: String,
        /// Répertoire du corpus.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Organisation des migrations dans le répertoire.
        #[arg(long, value_enum, default_value = "flat")]
        layout: corpus::Layout,
        /// Serveur PostgreSQL de test sur lequel rejouer les migrations
        /// (défaut : `PGWEAVE_ORACLE_URL`). Une base y est créée puis supprimée.
        #[arg(long)]
        pg_url: Option<String>,
        /// Fichier Markdown à écrire (défaut : sortie standard).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Affiche sur la sortie d'erreur le détail de chaque écart, noms
        /// compris. Pour le travail local uniquement : jamais écrit dans le
        /// rapport.
        #[arg(long)]
        details: bool,
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
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
