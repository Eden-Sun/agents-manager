/**
 * #1914：`status_json` 沒有 `fast_mode`（codex 的狀態列只有 context_window）要是 `null`，不是 `false`；
 * `runningFast` 才會回退到 `run.runtime_fast`。走真的 `toRun` 正規化，不手寫 Run。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { toRun, toStatusInfo } from './normalize.ts'
import { runningFast } from '../lib/runtimeDrift.ts'
import type { Bot, Run } from './types.ts'

const bot = (fast: boolean) => ({ id: 'b1', kind: 'codex', fast }) as Bot

/** daemon 的 run 形狀；`status_json` 是字串（DB 欄位原樣）。 */
const run = (statusJson: unknown, runtimeFast: unknown): Run =>
  toRun({ id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle', status_json: statusJson, runtime_fast: runtimeFast }) as Run

test('codex 的 status_json 只有 context_window：fast_mode 是 null，runningFast 回退到 runtime_fast（=1 → fast）', () => {
  const r = run(JSON.stringify({ context_window: { used_percentage: 12 } }), 1)
  assert.ok(r, 'run 正規化成功')
  assert.equal(r.status?.fast_mode, null, '缺欄位不能變成 false')
  assert.equal(runningFast(bot(false), r), true, '設定是關、實際在跑 fast：以 runtime_fast 為準')
})

test('codex 的 status_json 只有 context_window：runtime_fast=0 時是非 fast（回退後照實際值）', () => {
  const r = run(JSON.stringify({ context_window: { used_percentage: 12 } }), 0)
  assert.equal(runningFast(bot(true), r), false, '設定是開、實際沒在跑：以 runtime_fast 為準')
})

test('claude 的 status_json 帶 fast_mode:false：仍是 false，不被 runtime_fast 蓋掉', () => {
  const r = run(JSON.stringify({ fast_mode: false, context_window: {} }), 1)
  assert.equal(r.status?.fast_mode, false)
  assert.equal(runningFast({ id: 'b1', kind: 'claude', fast: true } as Bot, r), false, 'CLI 回報的值優先')
})

test('claude 的 status_json 帶 fast_mode:true：是 true', () => {
  const r = run(JSON.stringify({ fast_mode: true }), 0)
  assert.equal(r.status?.fast_mode, true)
  assert.equal(runningFast(bot(false), r), true)
})

test('非 boolean 的 fast_mode（字串、數字）不算回報：給 null，不照字面轉成 true', () => {
  assert.equal(toStatusInfo({ fast_mode: 'true' })?.fast_mode, null)
  assert.equal(toStatusInfo({ fast_mode: 1 })?.fast_mode, null)
  assert.equal(toStatusInfo({ fast_mode: null })?.fast_mode, null)
})
