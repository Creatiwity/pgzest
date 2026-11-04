/**
 * Declarative DSL for PostgreSQL Row Level Security policies.
 * Generates CREATE POLICY SQL from a fluent builder.
 *
 * @example
 * const policy = createPolicy('users_select')
 *   .on('users')
 *   .for('SELECT')
 *   .using('user_id = current_user_id()')
 *   .toSql()
 *
 * // -> CREATE POLICY users_select ON users FOR SELECT USING (user_id = current_user_id());
 */

export type PolicyCommand = 'ALL' | 'SELECT' | 'INSERT' | 'UPDATE' | 'DELETE'
export type PolicyPermissiveness = 'PERMISSIVE' | 'RESTRICTIVE'

export interface PolicyConfig {
  name: string
  table: string
  command: PolicyCommand
  permissiveness: PolicyPermissiveness
  roles: string[]
  usingExpr: string | null
  withCheckExpr: string | null
}

export class PolicyBuilder {
  private config: PolicyConfig

  constructor(name: string) {
    this.config = {
      name,
      table: '',
      command: 'ALL',
      permissiveness: 'PERMISSIVE',
      roles: [],
      usingExpr: null,
      withCheckExpr: null,
    }
  }

  /** Target table for this policy */
  on(table: string): this {
    this.config.table = table
    return this
  }

  /** SQL command this policy applies to */
  for(command: PolicyCommand): this {
    this.config.command = command
    return this
  }

  /** Make this policy RESTRICTIVE instead of PERMISSIVE */
  restrictive(): this {
    this.config.permissiveness = 'RESTRICTIVE'
    return this
  }

  /** Roles this policy applies to (default: PUBLIC) */
  toRoles(...roles: string[]): this {
    this.config.roles = roles
    return this
  }

  /** USING expression — filters rows for SELECT, UPDATE, DELETE */
  using(expr: string): this {
    this.config.usingExpr = expr
    return this
  }

  /** WITH CHECK expression — validates rows for INSERT and UPDATE */
  withCheck(expr: string): this {
    this.config.withCheckExpr = expr
    return this
  }

  /** Generate the CREATE POLICY SQL statement */
  toSql(): string {
    const { name, table, command, permissiveness, roles, usingExpr, withCheckExpr } = this.config
    if (!table) throw new Error('policy.on(table) is required')

    let sql = `CREATE POLICY ${quote(name)} ON ${quote(table)}`
    sql += ` AS ${permissiveness}`
    sql += ` FOR ${command}`
    if (roles.length > 0) {
      sql += ` TO ${roles.join(', ')}`
    }
    if (usingExpr) {
      sql += ` USING (${usingExpr})`
    }
    if (withCheckExpr) {
      sql += ` WITH CHECK (${withCheckExpr})`
    }
    sql += ';'
    return sql
  }

  /** Serialize to a JSON-compatible object for storage/inspection */
  toJson(): PolicyConfig {
    return { ...this.config }
  }

  /** Reconstruct a PolicyBuilder from a JSON config */
  static fromJson(config: PolicyConfig): PolicyBuilder {
    const builder = new PolicyBuilder(config.name)
    builder.config = { ...config }
    return builder
  }
}

/** Fluent entry point for building a policy */
export function createPolicy(name: string): PolicyBuilder {
  return new PolicyBuilder(name)
}

/** Common policy patterns as reusable templates */
export const policyTemplates = {
  /** Users can only see their own rows */
  ownRows: (table: string, userIdColumn: string, userFn = 'auth.uid()') =>
    createPolicy(`${table}_own_rows`)
      .on(table)
      .for('ALL')
      .using(`${userIdColumn} = ${userFn}`)
      .withCheck(`${userIdColumn} = ${userFn}`),

  /** All authenticated users can read */
  authenticatedRead: (table: string, userFn = 'auth.uid()') =>
    createPolicy(`${table}_read`)
      .on(table)
      .for('SELECT')
      .using(`${userFn} IS NOT NULL`),

  /** Org-scoped access (multi-tenant) */
  orgScoped: (table: string, orgIdColumn: string, orgFn: string) =>
    createPolicy(`${table}_org`)
      .on(table)
      .for('ALL')
      .using(`${orgIdColumn} IN (${orgFn})`)
      .withCheck(`${orgIdColumn} IN (${orgFn})`),

  /** Admin role bypass */
  adminBypass: (table: string) =>
    createPolicy(`${table}_admin`)
      .on(table)
      .for('ALL')
      .toRoles('admin')
      .using('true')
      .withCheck('true'),
}

function quote(name: string): string {
  // Only quote if name contains special chars or spaces
  return /^[a-z_][a-z0-9_]*$/i.test(name) ? name : `"${name}"`
}
