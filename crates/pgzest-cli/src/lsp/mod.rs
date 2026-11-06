//! `pgzest lsp`: the language server, over stdio.

mod backend;
mod completion;
mod line_index;
mod rust_embed;
mod semantic;

use backend::PgsqlBackend;
use tower_lsp::{LspService, Server};

pub fn run() -> i32 {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return 1;
        }
    };
    runtime.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        // `pgzest/expandMacro` is not in the standard protocol: register it as a
        // custom method, like `rust-analyzer/expandMacro`.
        let (service, socket) = LspService::build(PgsqlBackend::new)
            .custom_method("pgzest/expandMacro", PgsqlBackend::expand_macro)
            .finish();
        Server::new(stdin, stdout, socket).serve(service).await;
    });
    0
}
