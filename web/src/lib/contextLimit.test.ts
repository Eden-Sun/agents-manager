import { test } from 'node:test'
import assert from 'node:assert/strict'
import { compactThreshold } from './contextLimit'
import type { Project } from '../api/types'

const bot = (o: object = {}) => ({ kind: 'claude' as const, role: 'manager' as const, runtime_override: null, role_pending: false, ...o })
const project: Pick<Project, 'role_policy'> = { role_policy: { manager: { context_tokens: 400000 }, executor: { context_tokens: 300000 } } }

test('門檻順序：Bot 覆寫 → 角色 policy；沒設＝null', () => {
  assert.equal(compactThreshold(bot(), project), 400000)
  assert.equal(compactThreshold(bot({ runtime_override: { context_tokens: 600000 } }), project), 600000)
  assert.equal(compactThreshold(bot({ role: 'executor' }), project), 300000)
  assert.equal(compactThreshold(bot({ role: 'advisor' }), project), null)
})

test('還沒重啟（policy 改了）或 kind 不支援：不換分母', () => {
  assert.equal(compactThreshold(bot({ role_pending: true }), project), null, '舊 daemon 沒有 policy_pending：退回 role_pending')
  assert.equal(compactThreshold(bot({ policy_pending: true }), project), null)
  assert.equal(compactThreshold(bot({ kind: 'agy' }), project), null)
  assert.equal(compactThreshold(undefined, project), null)
})

/**
 * 2026-11-15 回歸：lead／L2 設 context_tokens = 500000（[1m] 模型，statusLine 回報視窗 1M）。部署換了 herdr shim → `role_pending`（含 shim 過期）亮，
 * 舊程式因此不套門檻、分母回到 1M。policy 沒變（policy_pending false）時，shim 過期不影響：分母仍是 500k。
 */
test('shim 過期（role_pending）但 policy 沒變：[1m] 模型＋context_tokens 500000 → 門檻 500000', () => {
  const p: Pick<Project, 'role_policy'> = { role_policy: { manager: { context_tokens: 500000 } } }
  assert.equal(compactThreshold(bot({ role_pending: true, policy_pending: false }), p), 500000)
  assert.equal(compactThreshold(bot({ role_pending: true, policy_pending: true }), p), null, 'policy 真的改了還沒重啟才不套')
})
