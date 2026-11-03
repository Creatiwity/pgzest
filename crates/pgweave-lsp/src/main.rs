use tower_lsp::{LspService, Server};
mod backend;
mod completion;
mod line_index;
mod rust_embed;
mod semantic;
use backend::PgsqlBackend;

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    // `pgweave/expandMacro` is not in the standard protocol: register it as a
    // custom method, like `rust-analyzer/expandMacro`.
    let (service, socket) = LspService::build(PgsqlBackend::new)
        .custom_method("pgweave/expandMacro", PgsqlBackend::expand_macro)
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;
}
