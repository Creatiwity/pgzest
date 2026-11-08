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

/**
 * The `pgzest` binary that runs the language server, in this order: the
 * `pgzest.server.path` setting, the binary shipped in a platform package
 * (`server/`), a build of this repository (extension in development, from
 * `editors/vscode/`), then `pgzest` on the PATH.
 */
function resolveServer(configured: string, extensionPath: string): string {
  if (configured !== '') {
    return configured;
  }
  const exe = process.platform === 'win32' ? 'pgzest.exe' : 'pgzest';
  const root = path.resolve(extensionPath, '..', '..');
  for (const candidate of [
    path.join(extensionPath, 'server', exe),
    path.join(root, 'target', 'debug', exe),
    path.join(root, 'target', 'release', exe),
  ]) {
    if (fs.existsSync(candidate)) return candidate;
  }
  return exe;
}

export function activate(context: vscode.ExtensionContext) {
  const config = vscode.workspace.getConfiguration('pgzest');
  const server = resolveServer(
    config.get<string>('server.path', ''),
    context.extensionPath,
  );
  const schemaFile = config.get<string>('schemaFile', '');

  const serverOptions: ServerOptions = {
    command: server,
    args: ['lsp'],
    transport: TransportKind.stdio,
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'pgsql' },
      { scheme: 'file', language: 'sql' },
      // Queries written in Rust code (`sql!`, `query!`): diagnostics, hover
      // and completion inside their string.
      { scheme: 'file', language: 'rust' },
    ],
    initializationOptions: { schemaFile },
    synchronize: {
      // Files created or changed outside the editor (`pgzest migrate
      // create`, git…): the server reloads the project. `*.json`: the
      // extension packs of `[postgres] packs`.
      fileEvents: vscode.workspace.createFileSystemWatcher(
        '**/{*.sql,*.pgsql,*.pgmacro,*.json,pgzest.toml}',
      ),
    },
  };

  client = new LanguageClient(
    'pgzest',
    'pgzest Language Server',
    serverOptions,
    clientOptions,
  );

  client.start();
  context.subscriptions.push({ dispose: () => client?.stop() });

  // "Show Macro Expansion" command: the client is passed through an accessor
  // because `activate` returns before the server is ready.
  registerExpandMacro(context, () => client);
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
