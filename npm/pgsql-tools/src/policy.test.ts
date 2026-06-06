// Tests for the policy DSL — no external deps required
import { createPolicy, policyTemplates, PolicyBuilder } from './policy'

function test(name: string, fn: () => void): void {
  try {
    fn()
    console.log(`  ok ${name}`)
  } catch (e) {
    console.error(`  FAIL ${name}: ${e}`)
    process.exitCode = 1
  }
}

function assertEqual(actual: string, expected: string): void {
  if (actual !== expected) {
    throw new Error(`Expected:\n${expected}\nGot:\n${actual}`)
  }
}

function assertContains(haystack: string, needle: string): void {
  if (!haystack.includes(needle)) {
    throw new Error(`Expected to contain "${needle}" in:\n${haystack}`)
  }
}

console.log('Policy DSL tests:')

test('basic SELECT policy', () => {
  const sql = createPolicy('users_select')
    .on('users')
    .for('SELECT')
    .using('user_id = current_user_id()')
    .toSql()
  assertContains(sql, 'CREATE POLICY users_select ON users')
  assertContains(sql, 'FOR SELECT')
  assertContains(sql, 'USING (user_id = current_user_id())')
})

test('RESTRICTIVE policy with WITH CHECK', () => {
  const sql = createPolicy('tasks_write')
    .on('tasks')
    .for('INSERT')
    .restrictive()
    .using('org_id = current_org_id()')
    .withCheck('org_id = current_org_id()')
    .toSql()
  assertContains(sql, 'AS RESTRICTIVE')
  assertContains(sql, 'WITH CHECK')
})

test('ownRows template', () => {
  const sql = policyTemplates.ownRows('tasks', 'user_id').toSql()
  assertContains(sql, 'CREATE POLICY tasks_own_rows ON tasks')
  assertContains(sql, 'user_id = auth.uid()')
})

test('orgScoped template', () => {
  const sql = policyTemplates.orgScoped('tasks', 'org_id', 'current_org_ids()').toSql()
  assertContains(sql, 'org_id IN (current_org_ids())')
})

test('toJson/fromJson roundtrip', () => {
  const original = createPolicy('test').on('t').for('ALL').using('true')
  const json = original.toJson()
  const restored = PolicyBuilder.fromJson(json)
  assertEqual(restored.toSql(), original.toSql())
})
