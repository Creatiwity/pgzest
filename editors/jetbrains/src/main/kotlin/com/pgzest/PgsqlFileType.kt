package com.pgzest

import com.intellij.lang.Language
import com.intellij.openapi.fileTypes.LanguageFileType
import javax.swing.Icon

object SqlLanguage : Language("SQL")

class PgsqlFileType : LanguageFileType(SqlLanguage) {
    override fun getName(): String = "SQL"
    override fun getDescription(): String = "PostgreSQL SQL file"
    override fun getDefaultExtension(): String = "sql"
    override fun getIcon(): Icon? = null  // Use IDE default SQL icon
}
