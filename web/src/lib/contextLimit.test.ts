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

test('還沒重啟（role_pending）或 kind 不支援：不換分母', () => {
  assert.equal(compactThreshold(bot({ role_pending: true }), project), null)
  assert.equal(compactThreshold(bot({ kind: 'agy' }), project), null)
  assert.equal(compactThreshold(undefined, project), null)
})
