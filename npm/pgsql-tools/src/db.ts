/**
 * Type-safe PostgreSQL client wrapping the `pg` pool.
 *
 * Usage:
 *   const db = new Db(process.env.DATABASE_URL)
 *   const users = await db.query<UserRow>('SELECT id, email FROM users WHERE active = $1', [true])
 *   await db.end()
 */

// We use a conditional import pattern so this file works when `pg` is not installed
// (e.g. in WASM-only environments). The interface is identical.

export interface DbConfig {
  host?: string
  port?: number
  database?: string
  user?: string
  password?: string
  connectionString?: string
  max?: number        // pool max connections (default 10)
  idleTimeoutMillis?: number
  connectionTimeoutMillis?: number
  ssl?: boolean | { rejectUnauthorized?: boolean }
}

export interface QueryResult<T> {
  rows: T[]
  rowCount: number
  fields: Array<{ name: string; dataTypeID: number }>
}

export interface TransactionClient {
  query<T = Record<string, unknown>>(sql: string, params?: unknown[]): Promise<T[]>
  queryOne<T = Record<string, unknown>>(sql: string, params?: unknown[]): Promise<T | null>
  execute(sql: string, params?: unknown[]): Promise<number>
}

/**
 * Main database client. Wraps a `pg` connection pool.
 * All methods are fully typed: pass your generated interface as the type parameter.
 *
 * @example
 * import { Db } from 'pgsql-tools'
 * import type { ProjectSummaryRow } from './generated/types'
 *
 * const db = new Db(process.env.DATABASE_URL!)
 * const rows = await db.query<ProjectSummaryRow>(
 *   'SELECT * FROM project_summary($1)',
 *   [orgId]
 * )
 */
export class Db {
  // pg Pool is loaded lazily to support WASM-only environments
  private _pool: unknown = null
  private _config: DbConfig | string

  constructor(config: DbConfig | string) {
    this._config = config
  }

  private async getPool(): Promise<PgPool> {
    if (!this._pool) {
      // Dynamic import so bundlers can tree-shake this in WASM environments
      const { Pool } = await import('pg' as string)
      this._pool = typeof this._config === 'string'
        ? new Pool({ connectionString: this._config })
        : new Pool(this._config)
    }
    return this._pool as PgPool
  }

  /**
   * Execute a query and return all rows typed as T.
   * Uses parameterized queries ($1, $2, ...) to prevent SQL injection.
   */
  async query<T = Record<string, unknown>>(
    sql: string,
    params?: unknown[]
  ): Promise<T[]> {
    const pool = await this.getPool()
    const result = await pool.query(sql, params)
    return result.rows as T[]
  }

  /**
   * Execute a query and return the first row, or null if no rows found.
   */
  async queryOne<T = Record<string, unknown>>(
    sql: string,
    params?: unknown[]
  ): Promise<T | null> {
    const rows = await this.query<T>(sql, params)
    return rows[0] ?? null
  }

  /**
   * Execute a statement (INSERT/UPDATE/DELETE) and return the number of affected rows.
   */
  async execute(sql: string, params?: unknown[]): Promise<number> {
    const pool = await this.getPool()
    const result = await pool.query(sql, params)
    return result.rowCount ?? 0
  }

  /**
   * Run a series of operations in a transaction.
   * Automatically COMMIT on success, ROLLBACK on error.
   *
   * @example
   * await db.transaction(async (tx) => {
   *   await tx.execute('INSERT INTO tasks (id) VALUES ($1)', [id])
   *   await tx.execute('UPDATE projects SET task_count = task_count + 1 WHERE id = $2', [projectId])
   * })
   */
  async transaction<T>(
    fn: (tx: TransactionClient) => Promise<T>
  ): Promise<T> {
    const pool = await this.getPool()
    const client = await pool.connect()
    try {
      await client.query('BEGIN')
      const tx: TransactionClient = {
        async query<R = Record<string, unknown>>(sql: string, params?: unknown[]) {
          const r = await client.query(sql, params)
          return r.rows as R[]
        },
        async queryOne<R = Record<string, unknown>>(sql: string, params?: unknown[]) {
          const r = await client.query(sql, params)
          return (r.rows[0] as R) ?? null
        },
        async execute(sql: string, params?: unknown[]) {
          const r = await client.query(sql, params)
          return r.rowCount ?? 0
        },
      }
      const result = await fn(tx)
      await client.query('COMMIT')
      return result
    } catch (err) {
      await client.query('ROLLBACK')
      throw err
    } finally {
      client.release()
    }
  }

  /** Close the connection pool gracefully. */
  async end(): Promise<void> {
    if (this._pool) {
      await (this._pool as PgPool).end()
      this._pool = null
    }
  }
}

// Minimal interface for the pg Pool (avoids importing pg types directly)
interface PgPool {
  query(sql: string, params?: unknown[]): Promise<{ rows: unknown[]; rowCount: number | null }>
  connect(): Promise<PgClient>
  end(): Promise<void>
}

interface PgClient {
  query(sql: string, params?: unknown[]): Promise<{ rows: unknown[]; rowCount: number | null }>
  release(): void
}
