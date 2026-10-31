import type { CheckResult, Diagnostic } from './types'

// When used with WASM, this module wraps the WASM API
// When used with napi, it wraps the native addon
// For now, it defines the interface — actual binding resolved at runtime

export interface CheckerBackend {
  check(sql: string, schemaSql: string): CheckResult
  lint(sql: string): Diagnostic[]
  migrateDiff(oldSql: string, newSql: string): string
  generateTypes?(sql: string, schemaSql: string): string
}

// Stub that throws until a backend is registered
let _backend: CheckerBackend | null = null

export function registerBackend(backend: CheckerBackend): void {
  _backend = backend
}

function getBackend(): CheckerBackend {
  if (!_backend) throw new Error('pgweave: no backend registered. Call registerBackend() first.')
  return _backend
}

export function check(sql: string, schemaSql: string): CheckResult {
  return getBackend().check(sql, schemaSql)
}

export function lint(sql: string): Diagnostic[] {
  return getBackend().lint(sql)
}

export function migrateDiff(oldSql: string, newSql: string): string {
  return getBackend().migrateDiff(oldSql, newSql)
}
