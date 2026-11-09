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

import { lastKnownModels, saveLastModels } from './modelsCache.ts'

function fakeStorage(broken = false) {
  const m = new Map<string, string>()
  const st = {
    get length() { return m.size },
    key: (i: number) => [...m.keys()][i] ?? null,
    getItem: (k: string) => { if (broken) throw new Error('blocked'); return m.get(k) ?? null },
    setItem: (k: string, v: string) => { if (broken) throw new Error('blocked'); m.set(k, v) },
  }
  ;(globalThis as { localStorage?: unknown }).localStorage = st
  return m
}
const info = (id: string) => ({ id, display_name: id, description: '', is_default: false, default_effort: null, efforts: [], service_tiers: [] }) as never

test('lastKnownModels: 成功寫入後讀回；沒有同 key 就用同 kind 最新的一份', () => {
  fakeStorage()
  assert.equal(lastKnownModels('codex', 'codex@local@'), null)
  saveLastModels('codex@local@', [info('gpt-6.1-luna')])
  assert.deepEqual(lastKnownModels('codex', 'codex@local@')?.map((m) => m.id), ['gpt-6.1-luna'])
  assert.deepEqual(lastKnownModels('codex', 'codex@m4p@')?.map((m) => m.id), ['gpt-6.1-luna'], '同 kind 任一份')
  assert.equal(lastKnownModels('claude', 'claude@local@'), null, '不同 kind 不借用')
})

test('lastKnownModels: localStorage 壞掉或內容損毀不炸，當沒有', () => {
  fakeStorage(true)
  saveLastModels('codex@local@', [info('x')])
  assert.equal(lastKnownModels('codex', 'codex@local@'), null)
  const m = fakeStorage()
  m.set('am.models.last:codex@local@', '{壞掉')
  assert.equal(lastKnownModels('codex', 'codex@local@'), null)
})
