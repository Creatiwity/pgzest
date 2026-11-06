package com.pgzest

import com.redhat.devtools.lsp4ij.server.ProcessStreamConnectionProvider

/**
 * Defines how JetBrains starts the pgzest language server (`pgzest lsp`).
 * The `pgzest` binary must be on the system PATH, or set by PGZEST_PATH.
 */
class PgsqlLspServerDefinition : ProcessStreamConnectionProvider() {
    override fun getCommands(): List<String> {
        val binary = System.getenv("PGZEST_PATH") ?: "pgzest"
        return listOf(binary, "lsp")
    }

    override fun getLanguageId(): String = "sql"
}
