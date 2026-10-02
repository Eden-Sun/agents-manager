import test from 'node:test'
import assert from 'node:assert/strict'
import { toRun } from './normalize.ts'
import { blockedReasonOf } from '../lib/blockedReason.ts'

const base = { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'blocked' }

test('run.blocked_reason {code, text} 讀進來；沒有／null／形狀不對都是 null，不壞（舊 daemon 沒這個欄位）', () => {
  assert.deepEqual(toRun({ ...base, blocked_reason: { code: 'codex_update_menu', text: 'codex 更新提示等待選擇' } })?.blocked_reason, {
    code: 'codex_update_menu',
    text: 'codex 更新提示等待選擇',
  })
  assert.equal(toRun({ ...base })?.blocked_reason, null, '舊 daemon：沒帶')
  assert.equal(toRun({ ...base, blocked_reason: null })?.blocked_reason, null)
  for (const bad of ['x', 3, [], {}, { code: 'a' }, { text: 'b' }, { code: '', text: '' }, { code: 1, text: 2 }]) {
    assert.equal(toRun({ ...base, blocked_reason: bad })?.blocked_reason, null, JSON.stringify(bad))
  }
})

test('blockedReasonOf：只有 run 現在真的是 blocked 才給文字，狀態一變就不再掛著舊原因', () => {
  const reason = { code: 'rate_limit_switch', text: 'codex 額度換模型建議等待選擇' }
  assert.equal(blockedReasonOf(toRun({ ...base, blocked_reason: reason })), reason.text)
  assert.equal(blockedReasonOf(toRun({ ...base, agent_status: 'idle', blocked_reason: reason })), '', '不是 blocked')
  assert.equal(blockedReasonOf(toRun({ ...base, state: 'exited', blocked_reason: reason })), '', 'run 已結束')
  assert.equal(blockedReasonOf(toRun({ ...base })), '')
  assert.equal(blockedReasonOf(null), '')
  assert.equal(blockedReasonOf(undefined), '')
})
