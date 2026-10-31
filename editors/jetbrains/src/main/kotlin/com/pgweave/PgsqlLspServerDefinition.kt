package com.pgweave

import com.redhat.devtools.lsp4ij.server.ProcessStreamConnectionProvider

/**
 * Defines how JetBrains connects to the pgweave-lsp language server.
 * The server binary must be on the system PATH (or configured via PGWEAVE_LSP_PATH).
 */
class PgsqlLspServerDefinition : ProcessStreamConnectionProvider() {
    override fun getCommands(): List<String> {
        // Look for pgweave-lsp on PATH; users can override in plugin settings
        val binary = System.getenv("PGWEAVE_LSP_PATH") ?: "pgweave-lsp"
        return listOf(binary)
    }

    override fun getLanguageId(): String = "sql"
}
