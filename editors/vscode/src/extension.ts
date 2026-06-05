import * as vscode from 'vscode'
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from 'vscode-languageclient/node'

let client: LanguageClient | undefined

export function activate(context: vscode.ExtensionContext): void {
  const config = vscode.workspace.getConfiguration('pgsqlTools')
  const lspPath = config.get<string>('lspPath', 'pgsql-lsp')

  const serverOptions: ServerOptions = {
    run: { command: lspPath, transport: TransportKind.stdio },
    debug: { command: lspPath, transport: TransportKind.stdio },
  }

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'sql' },
      { scheme: 'file', language: 'plpgsql' },
    ],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher('**/*.{sql,psql,plpgsql}'),
    },
  }

  client = new LanguageClient(
    'pgsql-tools',
    'pgsql-tools Language Server',
    serverOptions,
    clientOptions
  )

  client.start()

  context.subscriptions.push(
    vscode.commands.registerCommand('pgsqlTools.restartServer', async () => {
      await client?.restart()
      vscode.window.showInformationMessage('pgsql-tools LSP server restarted')
    })
  )
}

export function deactivate(): Promise<void> | undefined {
  return client?.stop()
}
