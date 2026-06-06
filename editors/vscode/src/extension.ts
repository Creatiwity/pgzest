import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

function resolveServerPath(configured: string, extensionPath: string): string {
  if (path.isAbsolute(configured) && fs.existsSync(configured)) {
    return configured;
  }
  // Walk up from editors/vscode/ to workspace root, try debug then release
  const root = path.resolve(extensionPath, '..', '..');
  for (const candidate of [
    path.join(root, 'target', 'debug', 'pgsql-lsp'),
    path.join(root, 'target', 'release', 'pgsql-lsp'),
  ]) {
    if (fs.existsSync(candidate)) return candidate;
  }
  return configured;
}

export function activate(context: vscode.ExtensionContext) {
  const config = vscode.workspace.getConfiguration('pgsql-tools');
  const configured = config.get<string>('lspPath', 'pgsql-lsp');
  const lspPath = resolveServerPath(configured, context.extensionPath);
  const schemaFile = config.get<string>('schemaFile', '');

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
    initializationOptions: { schemaFile },
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
