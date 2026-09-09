import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Run } from '../api/types.ts'
import { driftTitle, runtimeDrift, runtimeKnown } from './runtimeDrift.ts'

const bot = (over: Partial<Bot> = {}): Bot =>
  ({ id: 'b1', kind: 'codex', model: 'gpt-5.6-luna', effort: 'high', fast: true, ...over }) as Bot

const run = (over: Partial<Run> = {}): Run =>
  ({
    id: 'r1',
    bot_id: 'b1',
    state: 'running',
    agent_status: 'idle',
    runtime_model: 'gpt-5.6-luna',
    runtime_effort: 'high',
    runtime_fast: true,
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

test('不知道 runtime 就不要猜：舊 daemon、收編的 pane、沒在跑的 run', () => {
  const unknown = run({ runtime_model: null, runtime_effort: null, runtime_fast: null })
  assert.equal(runtimeKnown(unknown), false)
  assert.deepEqual(runtimeDrift(bot({ effort: 'low' }), unknown), [])
  assert.deepEqual(runtimeDrift(bot({ effort: 'low' }), run({ state: 'stopped' })), [])
  assert.deepEqual(runtimeDrift(bot(), null), [])
})
