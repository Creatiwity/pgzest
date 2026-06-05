export type Severity = 'error' | 'warning' | 'info'

export interface Diagnostic {
  severity: Severity
  code: string
  message: string
  hint?: string
}

export interface CheckResult {
  diagnostics: Diagnostic[]
  hasErrors: boolean
  errorCount: number
  warningCount: number
}

// Represents a SQL query with its inferred TypeScript return type
export interface TypedQuery<TResult> {
  sql: string
  // Placeholder type parameter — actual type is inferred at build time
  _resultType?: TResult
}
