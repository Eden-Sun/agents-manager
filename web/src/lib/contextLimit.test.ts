import { test } from 'node:test'
import assert from 'node:assert/strict'
import { compactThreshold } from './contextLimit'
import type { Project } from '../api/types'

const bot = (o: object = {}) => ({ kind: 'claude' as const, role: 'manager' as const, runtime_override: null, role_pending: false, share_profile: null, ...o })
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
  assert.equal(compactThreshold(bot({ kind: 'agy', role: null }), undefined), 400000, 'agy 沒覆寫、沒 policy：預設 400k')
  assert.equal(compactThreshold(bot({ kind: 'agy', role: null, runtime_override: { context_tokens: 300000 } }), undefined), 300000, '覆寫優先')
  assert.equal(compactThreshold(bot({ kind: 'claude', role: null }), undefined), null, 'claude 不設預設')
  assert.equal(compactThreshold(bot({ kind: 'agy', role: null, policy_pending: true }), undefined), null, 'policy 待套用不換分母')
  assert.equal(compactThreshold(undefined, project), null)
})

test('分享功能 bot：預設 400k，覆寫與角色 policy 優先，待套用回 null', () => {
  assert.equal(compactThreshold(bot({ kind: 'claude', role: null, share_profile: 'restricted' }), undefined), 400000)
  assert.equal(compactThreshold(bot({ kind: 'claude', role: null, share_profile: 'trusted' }), undefined), 400000)
  assert.equal(compactThreshold(bot({ kind: 'claude', role: null, share_profile: 'restricted', runtime_override: { context_tokens: 500000 } }), undefined), 500000)
  assert.equal(compactThreshold(bot({ kind: 'claude', role: 'executor', share_profile: 'restricted' }), project), 300000)
  assert.equal(compactThreshold(bot({ kind: 'claude', role: null, share_profile: 'trusted', policy_pending: true }), undefined), null)
})

test('shim 過期（role_pending）但 policy 沒變：[1m] 模型＋context_tokens 500000 → 門檻 500000', () => {
  const p: Pick<Project, 'role_policy'> = { role_policy: { manager: { context_tokens: 500000 } } }
  assert.equal(compactThreshold(bot({ role_pending: true, policy_pending: false }), p), 500000)
  assert.equal(compactThreshold(bot({ role_pending: true, policy_pending: true }), p), null, 'policy 真的改了還沒重啟才不套')
})
