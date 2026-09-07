import test from 'node:test'
import assert from 'node:assert/strict'
import { dropHostModels, MODELS_RETRY_MS, modelsKey, modelsKeyHost, shouldFetchModels } from './modelsCache.ts'

test('modelsKey / modelsKeyHost round-trip, empty host means local', () => {
  assert.equal(modelsKey('claude', '', null), 'claude@local@')
  assert.equal(modelsKey('codex', 'm4p', 'work'), 'codex@m4p@work')
  assert.equal(modelsKeyHost('codex@m4p@work'), 'm4p')
  assert.equal(modelsKeyHost('claude@local@'), 'local')
})

test('shouldFetchModels: never fetched → fetch; have list → no', () => {
  assert.equal(shouldFetchModels(undefined, undefined, 1000), true)
  assert.equal(shouldFetchModels([], undefined, 1000), false)
  assert.equal(shouldFetchModels([{ id: 'x', label: 'x' } as never], 0, 1000), false)
})

test('shouldFetchModels: a failure is retried only after the cooldown (issue #26)', () => {
  const at = 10_000
  assert.equal(shouldFetchModels(null, at, at + 1), false)
  assert.equal(shouldFetchModels(null, at, at + MODELS_RETRY_MS - 1), false)
  assert.equal(shouldFetchModels(null, at, at + MODELS_RETRY_MS), true)
  // null with no timestamp (legacy shape) is retried right away
  assert.equal(shouldFetchModels(null, undefined, at), true)
})

test('dropHostModels removes only that host, keeps identity of untouched cache', () => {
  const cache = { 'claude@m4p@': null, 'codex@m4p@a': [], 'claude@local@': [], 'claude@other@': null }
  const next = dropHostModels(cache, 'm4p')
  assert.deepEqual(Object.keys(next).sort(), ['claude@local@', 'claude@other@'])
  assert.equal(dropHostModels(next, 'm4p'), next)
  assert.deepEqual(Object.keys(dropHostModels(cache, '')), ['claude@m4p@', 'codex@m4p@a', 'claude@other@'])
})
