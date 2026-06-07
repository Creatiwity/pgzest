use tower_lsp::{LspService, Server};
mod backend;
mod completion;
mod line_index;
mod semantic;
use backend::PgsqlBackend;

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(PgsqlBackend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
