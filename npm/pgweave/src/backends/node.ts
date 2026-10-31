/**
 * Backend implementation using the native napi-rs addon (pgweave-node).
 * Best performance — requires a native .node binary.
 *
 * @example
 * import { registerBackend } from 'pgweave'
 * import { NodeBackend } from 'pgweave/backends/node'
 * registerBackend(new NodeBackend())
 */

import type { CheckerBackend } from '../checker'
import type { Diagnostic, CheckResult } from '../types'

interface NativeAddon {
  check(sql: string, schemaSql: string): Array<{ severity: string; code: string; message: string }>
  lint(sql: string): Array<{ severity: string; code: string; message: string }>
  migrateDiff(oldSql: string, newSql: string): string
  generateTypes(sql: string, schemaSql: string): string
}

export class NodeBackend implements CheckerBackend {
  private addon: NativeAddon

  constructor(addonPath?: string) {
    const path = addonPath ?? 'pgweave-node'
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    this.addon = require(path) as NativeAddon
  }

  check(sql: string, schemaSql: string): CheckResult {
    const raw = this.addon.check(sql, schemaSql)
    const diagnostics: Diagnostic[] = raw.map(d => ({
      severity: d.severity as Diagnostic['severity'],
      code: d.code,
      message: d.message,
    }))
    return {
      diagnostics,
      hasErrors: diagnostics.some(d => d.severity === 'error'),
      errorCount: diagnostics.filter(d => d.severity === 'error').length,
      warningCount: diagnostics.filter(d => d.severity === 'warning').length,
    }
  }

  lint(sql: string): Diagnostic[] {
    return this.addon.lint(sql).map(d => ({
      severity: d.severity as Diagnostic['severity'],
      code: d.code,
      message: d.message,
    }))
  }

  migrateDiff(oldSql: string, newSql: string): string {
    return this.addon.migrateDiff(oldSql, newSql)
  }

  generateTypes(sql: string, schemaSql: string): string {
    return this.addon.generateTypes(sql, schemaSql)
  }
}
