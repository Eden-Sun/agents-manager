import test from 'node:test'
import assert from 'node:assert/strict'
import { toBot, toProject } from './normalize'

const base = { id: 'p1', path: '/x', label: 'x', mode: 'expert', host: 'local', created_at: '' }

test('舊 /api/state 的 executor_pool 與 used 欄位被忽略', () => {
  const p = toProject({
    ...base,
    executor_pool: [{ id: 'legacy', kind: 'claude', count: 2, used: 1 }],
    role_policy: { executor: { context_tokens: 300000 } },
  })
  assert.ok(p)
  assert.equal('executor_pool' in p, false)
  assert.deepEqual(p.role_policy, { executor: { context_tokens: 300000 } })
})

test('舊 Bot 的 pool_entry 欄位被忽略', () => {
  const bot = toBot({ id: 'b1', project_id: 'p1', pool_entry: 'legacy' })
  assert.ok(bot)
  assert.equal('pool_entry' in bot, false)
})
