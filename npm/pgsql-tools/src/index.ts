// Core types
export type { Diagnostic, Severity, CheckResult } from './types'

// Database client
export { Db } from './db'
export type { DbConfig, QueryResult, TransactionClient } from './db'

// Checker / Linter
export { check, lint, migrateDiff, registerBackend } from './checker'
export type { CheckerBackend } from './checker'

// Migration utilities
export { generateMigrationSql, applyMigration } from './migrate'
export type { MigrateOptions, MigrationResult, MigrationOperation } from './migrate'

// RLS Policy DSL
export { createPolicy, policyTemplates, PolicyBuilder } from './policy'
export type { PolicyConfig, PolicyCommand, PolicyPermissiveness } from './policy'

// TypeScript type generation helpers
export { generateTypes, pgTypeToTs } from './codegen'
export type { ColumnTyping } from './codegen'
