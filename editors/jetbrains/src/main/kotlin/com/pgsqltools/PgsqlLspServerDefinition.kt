package com.pgsqltools

import com.redhat.devtools.lsp4ij.server.ProcessStreamConnectionProvider

/**
 * Defines how JetBrains connects to the pgsql-lsp language server.
 * The server binary must be on the system PATH (or configured via PGSQL_LSP_PATH).
 */
class PgsqlLspServerDefinition : ProcessStreamConnectionProvider() {
    override fun getCommands(): List<String> {
        // Look for pgsql-lsp on PATH; users can override in plugin settings
        val binary = System.getenv("PGSQL_LSP_PATH") ?: "pgsql-lsp"
        return listOf(binary)
    }

    override fun getLanguageId(): String = "sql"
}
