import { fromPrismaSchema } from './plugins/prisma'

function test(name: string, fn: () => void): void {
  try { fn(); console.log(`  ok ${name}`) }
  catch (e) { console.error(`  FAIL ${name}: ${e}`); process.exitCode = 1 }
}
function assertContains(h: string, n: string): void {
  if (!h.includes(n)) throw new Error(`Expected "${n}" in:\n${h}`)
}

const schema = `
  model User {
    id        String   @id @default(uuid())
    email     String   @unique
    name      String?
    createdAt DateTime @default(now()) @map("created_at")
    tasks     Task[]
    @@map("users")
  }

  model Task {
    id     String @id @default(uuid())
    title  String
    done   Boolean @default(false)
    userId String  @map("user_id")
    @@map("tasks")
  }
`

console.log('Prisma plugin tests:')

test('parses models correctly', () => {
  const result = fromPrismaSchema(schema)
  if (result.models.length < 2) throw new Error('expected 2 models')
  const user = result.models.find(m => m.name === 'User')
  if (!user) throw new Error('User model not found')
  if (user.tableName !== 'users') throw new Error(`expected tableName users, got ${user.tableName}`)
})

test('generates DDL', () => {
  const { ddl } = fromPrismaSchema(schema)
  assertContains(ddl, 'CREATE TABLE users')
  assertContains(ddl, 'CREATE TABLE tasks')
  assertContains(ddl, 'email TEXT')
  assertContains(ddl, 'PRIMARY KEY')
})

test('generates TypeScript types', () => {
  const { types } = fromPrismaSchema(schema)
  assertContains(types, 'export interface User {')
  assertContains(types, 'id: string;')
  assertContains(types, 'name: string | null;')
  assertContains(types, 'export interface Task {')
})
