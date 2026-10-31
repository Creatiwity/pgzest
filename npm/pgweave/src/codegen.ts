// Type generation from SQL query results
// At build time, the checker produces column info; this module turns it into TS types.

export interface ColumnTyping {
  name: string
  tsType: string
  nullable: boolean
}

export interface TypedQuery<T = Record<string, unknown>> {
  sql: string
  columns: ColumnTyping[]
}

// Maps PostgreSQL types to TypeScript types
export function pgTypeToTs(pgType: string): string {
  const map: Record<string, string> = {
    'uuid': 'string',
    'text': 'string',
    'varchar': 'string',
    'char': 'string',
    'int2': 'number',
    'int4': 'number',
    'integer': 'number',
    'int8': 'number',
    'bigint': 'number',
    'numeric': 'number',
    'float4': 'number',
    'float8': 'number',
    'bool': 'boolean',
    'boolean': 'boolean',
    'timestamp': 'Date',
    'timestamptz': 'Date',
    'date': 'Date',
    'jsonb': 'unknown',
    'json': 'unknown',
  }
  return map[pgType.toLowerCase()] ?? 'unknown'
}

// Generates a TypeScript interface string from column typings
export function generateTypes(
  queryName: string,
  columns: ColumnTyping[]
): string {
  const fields = columns
    .map(c => `  ${c.name}${c.nullable ? '?' : ''}: ${c.tsType};`)
    .join('\n')
  return `export interface ${queryName} {\n${fields}\n}`
}
