/**
 * SQL migration utilities.
 * Generates migration SQL from schema diffs using the Rust codegen engine.
 */

import type { CheckerBackend } from './checker'

export interface MigrateOptions {
  /** SQL source of the old schema (or empty string for initial migration) */
  fromSql: string
  /** SQL source of the new (target) schema */
  toSql: string
  /** If true, return the SQL without executing it */
  dryRun?: boolean
}

export interface MigrationResult {
  sql: string
  operations: MigrationOperation[]
  appliedAt?: Date
}

export interface MigrationOperation {
  kind: 'create_table' | 'drop_table' | 'add_column' | 'drop_column' | 'create_index' | 'drop_index' | 'create_function' | 'drop_function' | 'other'
  objectName: string
  sql: string
}

/**
 * Generate migration SQL from two schema versions.
 * Delegates to the Rust `pgsql-codegen` engine for accurate diff.
 *
 * @example
 * import { generateMigrationSql } from 'pgsql-tools'
 *
 * const { sql } = await generateMigrationSql({
 *   fromSql: await fs.readFile('schema.v1.sql', 'utf8'),
 *   toSql: await fs.readFile('schema.v2.sql', 'utf8'),
 * }, backend)
 * console.log(sql) // ALTER TABLE ... ADD COLUMN ...
 */
export async function generateMigrationSql(
  options: MigrateOptions,
  backend: CheckerBackend
): Promise<MigrationResult> {
  const rawSql = backend.migrateDiff(options.fromSql, options.toSql)

  // Parse the diff output into individual operations
  const operations = parseMigrationOps(rawSql)

  return {
    sql: rawSql,
    operations,
  }
}

/**
 * Apply a migration to a database.
 * Wraps the SQL in a transaction for safety.
 *
 * @example
 * import { Db, applyMigration } from 'pgsql-tools'
 *
 * const db = new Db(process.env.DATABASE_URL!)
 * await applyMigration(db, migrationSql)
 */
export async function applyMigration(
  db: import('./db').Db,
  sql: string
): Promise<void> {
  if (!sql.trim()) return
  await db.transaction(async (tx) => {
    // Split on semicolons, filter empty statements, execute each
    const statements = sql
      .split(';')
      .map(s => s.trim())
      .filter(s => s.length > 0)
    for (const stmt of statements) {
      await tx.execute(stmt)
    }
  })
}

function parseMigrationOps(sql: string): MigrationOperation[] {
  const ops: MigrationOperation[] = []
  const stmts = sql.split(';').map(s => s.trim()).filter(Boolean)
  for (const stmt of stmts) {
    const upper = stmt.toUpperCase()
    let kind: MigrationOperation['kind'] = 'other'
    let objectName = ''

    if (upper.startsWith('CREATE TABLE')) {
      kind = 'create_table'
      objectName = extractObjectName(stmt, 'CREATE TABLE')
    } else if (upper.startsWith('DROP TABLE')) {
      kind = 'drop_table'
      objectName = extractObjectName(stmt, 'DROP TABLE')
    } else if (upper.startsWith('ALTER TABLE') && upper.includes('ADD COLUMN')) {
      kind = 'add_column'
      objectName = extractObjectName(stmt, 'ALTER TABLE')
    } else if (upper.startsWith('ALTER TABLE') && upper.includes('DROP COLUMN')) {
      kind = 'drop_column'
      objectName = extractObjectName(stmt, 'ALTER TABLE')
    } else if (upper.startsWith('CREATE INDEX') || upper.startsWith('CREATE UNIQUE INDEX')) {
      kind = 'create_index'
      objectName = extractObjectName(stmt, 'CREATE')
    } else if (upper.startsWith('DROP INDEX')) {
      kind = 'drop_index'
      objectName = extractObjectName(stmt, 'DROP INDEX')
    } else if (upper.startsWith('CREATE') && (upper.includes('FUNCTION') || upper.includes('PROCEDURE'))) {
      kind = 'create_function'
      objectName = extractFunctionName(stmt)
    } else if (upper.startsWith('DROP') && (upper.includes('FUNCTION') || upper.includes('PROCEDURE'))) {
      kind = 'drop_function'
      objectName = extractFunctionName(stmt)
    }

    ops.push({ kind, objectName, sql: stmt })
  }
  return ops
}

function extractObjectName(sql: string, prefix: string): string {
  const rest = sql.slice(prefix.length).trim()
  const match = rest.match(/^(?:IF\s+(?:NOT\s+)?EXISTS\s+)?(["'\w.]+)/i)
  return match?.[1]?.replace(/"/g, '') ?? ''
}

function extractFunctionName(sql: string): string {
  const match = sql.match(/(?:FUNCTION|PROCEDURE)\s+(["'\w.]+)/i)
  return match?.[1]?.replace(/"/g, '') ?? ''
}
