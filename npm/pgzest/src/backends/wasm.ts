/**
 * Backend implementation using the WebAssembly module (pgzest-wasm).
 * Works in browser and edge runtimes — no native binaries required.
 *
 * @example
 * import init, { check as wasmCheck } from 'pgzest-wasm'
 * import { registerBackend } from 'pgzest'
 * import { WasmBackend } from 'pgzest/backends/wasm'
 *
 * await init()
 * registerBackend(new WasmBackend({ check: wasmCheck, ... }))
 */

import type { CheckerBackend } from '../checker'
import type { Diagnostic, CheckResult } from '../types'

export interface WasmModule {
  check(sql: string, schemaSql: string): { diagnostics_json(): string; has_errors(): boolean; error_count(): number; warning_count(): number }
  lint(sql: string): string
  migrate_diff(oldSql: string, newSql: string): string
  generate_types(sql: string, schemaSql: string): string
}

export class WasmBackend implements CheckerBackend {
  private wasm: WasmModule

  constructor(wasm: WasmModule) {
    this.wasm = wasm
  }

  check(sql: string, schemaSql: string): CheckResult {
    const result = this.wasm.check(sql, schemaSql)
    let diagnostics: Diagnostic[] = []
    try {
      diagnostics = JSON.parse(result.diagnostics_json())
    } catch {
      // ignore parse errors — return empty diagnostics
    }
    return {
      diagnostics,
      hasErrors: result.has_errors(),
      errorCount: result.error_count(),
      warningCount: result.warning_count(),
    }
  }

  lint(sql: string): Diagnostic[] {
    try {
      return JSON.parse(this.wasm.lint(sql)) as Diagnostic[]
    } catch {
      return []
    }
  }

  migrateDiff(oldSql: string, newSql: string): string {
    return this.wasm.migrate_diff(oldSql, newSql)
  }

  generateTypes(sql: string, schemaSql: string): string {
    return this.wasm.generate_types(sql, schemaSql)
  }
}
