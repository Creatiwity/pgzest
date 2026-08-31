/**
 * Commande « Voir l'expansion de la macro ».
 *
 * Même mécanique que « Expand macro recursively » de Rust Analyzer : une
 * requête LSP custom rend le SQL désucré, affiché dans un onglet en lecture
 * seule via un TextDocumentContentProvider sur un schéma d'URI dédié. En
 * lecture seule parce que ce contenu est engendré : l'éditer n'aurait aucun
 * effet sur le `.pgsql`, et laisser croire le contraire serait un piège.
 */
import * as vscode from 'vscode';
import { LanguageClient } from 'vscode-languageclient/node';

/** Schéma d'URI des documents d'expansion. */
export const EXPANDED_SCHEME = 'pgsql-expanded';

interface ExpandMacroResult {
  sql: string;
  expanded: number;
  note?: string;
}

/**
 * Fournit le contenu des onglets d'expansion.
 *
 * Le contenu est mémorisé par URI : VS Code peut redemander le document
 * (changement de thème, split, rechargement) et doit retrouver la même chose.
 */
class ExpansionProvider implements vscode.TextDocumentContentProvider {
  private readonly contents = new Map<string, string>();
  private readonly emitter = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.emitter.event;

  set(uri: vscode.Uri, text: string): void {
    this.contents.set(uri.toString(), text);
    // Prévenir VS Code, sinon un onglet déjà ouvert garde l'ancien contenu.
    this.emitter.fire(uri);
  }

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.contents.get(uri.toString()) ?? '';
  }

  dispose(): void {
    this.emitter.dispose();
    this.contents.clear();
  }
}

/**
 * Signale les macros d'un `.pgsql` par un lien « Voir l'expansion ».
 *
 * Complète le raccourci clavier : sans le CodeLens, la fonctionnalité n'est
 * découvrable qu'en connaissant déjà son existence.
 */
class ExpandCodeLensProvider implements vscode.CodeLensProvider {
  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    const lenses: vscode.CodeLens[] = [];
    for (let line = 0; line < document.lineCount; line++) {
      const text = document.lineAt(line).text;
      const match = /^\s*@([A-Za-z_][A-Za-z0-9_]*)/.exec(text);
      if (!match) continue;
      lenses.push(
        new vscode.CodeLens(new vscode.Range(line, 0, line, 0), {
          title: `Voir l'expansion de @${match[1]}`,
          command: 'pgsql-tools.expandMacro',
          arguments: [new vscode.Position(line, match[0].length)],
        }),
      );
    }
    return lenses;
  }
}

/** Enregistre la commande, le fournisseur de contenu et le CodeLens. */
export function registerExpandMacro(
  context: vscode.ExtensionContext,
  getClient: () => LanguageClient | undefined,
): void {
  const provider = new ExpansionProvider();

  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(EXPANDED_SCHEME, provider),
    provider,
    vscode.languages.registerCodeLensProvider(
      [
        { scheme: 'file', language: 'pgsql' },
        { scheme: 'file', language: 'sql' },
      ],
      new ExpandCodeLensProvider(),
    ),
    vscode.commands.registerCommand(
      'pgsql-tools.expandMacro',
      async (position?: vscode.Position) => {
        const client = getClient();
        const editor = vscode.window.activeTextEditor;
        if (!client || !editor) {
          vscode.window.showWarningMessage('pgsql-tools : aucun éditeur actif.');
          return;
        }

        // Position venant du CodeLens, sinon celle du curseur.
        const at = position ?? editor.selection.active;

        let result: ExpandMacroResult;
        try {
          result = await client.sendRequest<ExpandMacroResult>('pgsql/expandMacro', {
            textDocument: { uri: editor.document.uri.toString() },
            position: { line: at.line, character: at.character },
          });
        } catch (err) {
          vscode.window.showErrorMessage(`pgsql-tools : expansion impossible — ${err}`);
          return;
        }

        if (!result.sql) {
          vscode.window.showInformationMessage(
            `pgsql-tools : ${result.note ?? 'rien à développer ici.'}`,
          );
          return;
        }

        // Une URI par document source : réutiliser le même onglet plutôt que
        // d'en empiler un par invocation de la commande.
        const name = editor.document.uri.path.split('/').pop() ?? 'macro';
        const uri = vscode.Uri.parse(
          `${EXPANDED_SCHEME}:/${name}.expanded.sql?${encodeURIComponent(
            editor.document.uri.toString(),
          )}`,
        );

        const header =
          `-- Expansion engendrée par pgsql-tools — lecture seule.\n` +
          `-- Source : ${name}\n` +
          (result.note ? `-- ${result.note}\n` : '') +
          `\n`;
        provider.set(uri, header + result.sql);

        const doc = await vscode.workspace.openTextDocument(uri);
        await vscode.languages.setTextDocumentLanguage(doc, 'sql');
        await vscode.window.showTextDocument(doc, {
          preview: true,
          viewColumn: vscode.ViewColumn.Beside,
          preserveFocus: true,
        });
      },
    ),
  );
}
