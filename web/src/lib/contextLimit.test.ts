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
  assert.equal(compactThreshold(bot({ kind: 'grok' }), project), null)
  assert.equal(compactThreshold(bot({ kind: 'agy' }), project), 400000)
  assert.equal(compactThreshold(undefined, project), null)
})

test('shim 過期（role_pending）但 policy 沒變：[1m] 模型＋context_tokens 500000 → 門檻 500000', () => {
  const p: Pick<Project, 'role_policy'> = { role_policy: { manager: { context_tokens: 500000 } } }
  assert.equal(compactThreshold(bot({ role_pending: true, policy_pending: false }), p), 500000)
  assert.equal(compactThreshold(bot({ role_pending: true, policy_pending: true }), p), null, 'policy 真的改了還沒重啟才不套')
})
