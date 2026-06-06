import * as vscode from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

export function activate(context: vscode.ExtensionContext) {
  const config = vscode.workspace.getConfiguration('pgsql-tools');
  const lspPath = config.get<string>('lspPath', 'pgsql-lsp');

  const serverOptions: ServerOptions = {
    command: lspPath,
    args: [],
    transport: TransportKind.stdio,
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'sql' },
      { scheme: 'file', pattern: '**/*.pgsql' },
    ],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher('**/*.sql'),
    },
  };

  client = new LanguageClient(
    'pgsql-tools',
    'pgsql-tools Language Server',
    serverOptions,
    clientOptions,
  );

  client.start();
  context.subscriptions.push({ dispose: () => client?.stop() });
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
