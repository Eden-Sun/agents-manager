import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Run } from '../api/types.ts'
import { driftTitle, isFastOnlyDrift, runtimeDrift, runtimeKnown } from './runtimeDrift.ts'

const bot = (over: Partial<Bot> = {}): Bot =>
  ({ id: 'b1', kind: 'codex', model: 'gpt-5.6-luna', effort: 'high', fast: true, identity: null, ...over }) as Bot

const run = (over: Partial<Run> = {}): Run =>
  ({
    id: 'r1',
    bot_id: 'b1',
    state: 'running',
    agent_status: 'idle',
    runtime_model: 'gpt-5.6-luna',
    runtime_effort: 'high',
    runtime_fast: true,
    runtime_identity: '',
    ...over,
  }) as Run

test('設定與 runtime 一致時什麼都不標', () => {
  assert.deepEqual(runtimeDrift(bot(), run()), [])
})

test('codex 改了強度沒重啟：標出實際在跑的那個值', () => {
  // 實況：AG Man 寫 High，終端裡 codex 印的是 xhigh。
  const d = runtimeDrift(bot({ effort: 'high' }), run({ runtime_effort: 'xhigh' }))
  assert.equal(d.length, 1)
  assert.equal(d[0].field, 'effort')
  assert.equal(d[0].running, 'Xhigh')
  assert.equal(d[0].configured, 'High')
  assert.match(driftTitle(d), /重啟/)
})

test('模型與 fast 也一起比，清成 CLI 預設同樣算不一致', () => {
  const d = runtimeDrift(bot({ model: null, fast: false }), run())
  assert.deepEqual(
    d.map((x) => x.field),
    ['model', 'fast'],
  )
  assert.equal(d[0].configured, '（CLI 預設）')
})

test('fast 只有 codex 會變成啟動旗標，其他 kind 不比', () => {
  assert.deepEqual(runtimeDrift(bot({ kind: 'grok', fast: false }), run({ runtime_fast: true })), [])
})

test('身份相同時不標 drift，即使其他 runtime 欄位未知', () => {
  const actual = run({ runtime_model: null, runtime_effort: null, runtime_fast: null, runtime_identity: 'cc1' })
  assert.equal(runtimeKnown(actual), true)
  assert.deepEqual(runtimeDrift(bot({ identity: 'cc1' }), actual), [])
})

test('身份不同時標出實際帳號與設定帳號', () => {
  const d = runtimeDrift(
    bot({ kind: 'claude', identity: 'cc1' }),
    run({ runtime_model: null, runtime_effort: null, runtime_fast: null, runtime_identity: '' }),
  )
  assert.equal(d.length, 1)
  assert.equal(d[0].field, 'identity')
  assert.equal(d[0].label, '帳號')
  assert.equal(d[0].running, 'cc0')
  assert.equal(d[0].configured, 'cc1')
})

test('runtime_identity 為 null 時身份未知，不猜也不標 drift', () => {
  const unknown = run({ runtime_model: null, runtime_effort: null, runtime_fast: null, runtime_identity: null })
  assert.equal(runtimeKnown(unknown), false)
  assert.deepEqual(runtimeDrift(bot({ identity: 'cc1' }), unknown), [])
})

test('不知道 runtime 就不要猜：收編的 pane、沒在跑的 run', () => {
  const unknown = run({ runtime_model: null, runtime_effort: null, runtime_fast: null, runtime_identity: null })
  assert.equal(runtimeKnown(unknown), false)
  assert.deepEqual(runtimeDrift(bot({ effort: 'low' }), unknown), [])
  assert.deepEqual(runtimeDrift(bot({ effort: 'low' }), run({ state: 'stopped' })), [])
  assert.deepEqual(runtimeDrift(bot(), null), [])
})

test('#393：只有 codex 的 fast 落差才走當場套用；混了別的欄位、或不是 codex 仍走重啟', () => {
  const r = run({ runtime_fast: true })
  const fastOnly = runtimeDrift(bot({ fast: false }), r)
  assert.deepEqual(fastOnly.map((d) => d.field), ['fast'])
  assert.equal(isFastOnlyDrift('codex', fastOnly), true)
  assert.equal(isFastOnlyDrift('claude', fastOnly), false)
  const mixed = runtimeDrift(bot({ fast: false, effort: 'low' }), r)
  assert.equal(isFastOnlyDrift('codex', mixed), false)
  assert.equal(isFastOnlyDrift('codex', []), false)
})
