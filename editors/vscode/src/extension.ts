import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from 'vscode-languageclient/node';

import { registerExpandMacro } from './expand';

let client: LanguageClient | undefined;

function resolveServerPath(configured: string, extensionPath: string): string {
  if (path.isAbsolute(configured) && fs.existsSync(configured)) {
    return configured;
  }
  // Walk up from editors/vscode/ to workspace root, try debug then release
  const root = path.resolve(extensionPath, '..', '..');
  for (const candidate of [
    path.join(root, 'target', 'debug', 'pgweave-lsp'),
    path.join(root, 'target', 'release', 'pgweave-lsp'),
  ]) {
    if (fs.existsSync(candidate)) return candidate;
  }
  return configured;
}

export function activate(context: vscode.ExtensionContext) {
  const config = vscode.workspace.getConfiguration('pgweave');
  const configured = config.get<string>('lspPath', 'pgweave-lsp');
  const lspPath = resolveServerPath(configured, context.extensionPath);
  const schemaFile = config.get<string>('schemaFile', '');

  const serverOptions: ServerOptions = {
    command: lspPath,
    args: [],
    transport: TransportKind.stdio,
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'pgsql' },
      { scheme: 'file', language: 'sql' },
    ],
    initializationOptions: { schemaFile },
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher('**/*.{sql,pgsql}'),
    },
  };

  client = new LanguageClient(
    'pgweave',
    'pgweave Language Server',
    serverOptions,
    clientOptions,
  );

  client.start();
  context.subscriptions.push({ dispose: () => client?.stop() });

  // Commande « Voir l'expansion » : le client est passé par accesseur car
  // `activate` s'achève avant que le serveur soit prêt.
  registerExpandMacro(context, () => client);
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
