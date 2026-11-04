package com.pgzest

import com.redhat.devtools.lsp4ij.server.ProcessStreamConnectionProvider

/**
 * Defines how JetBrains connects to the pgzest-lsp language server.
 * The server binary must be on the system PATH (or configured via PGZEST_LSP_PATH).
 */
class PgsqlLspServerDefinition : ProcessStreamConnectionProvider() {
    override fun getCommands(): List<String> {
        // Look for pgzest-lsp on PATH; users can override in plugin settings
        val binary = System.getenv("PGZEST_LSP_PATH") ?: "pgzest-lsp"
        return listOf(binary)
    }

    override fun getLanguageId(): String = "sql"
}
