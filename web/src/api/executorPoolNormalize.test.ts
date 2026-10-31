import test from 'node:test'
import assert from 'node:assert/strict'
import { toProject } from './normalize'

const base = { id: 'p1', path: '/x', label: 'x', mode: 'expert', host: 'local', created_at: '' }

test('executor_pool：陣列形狀逐條保留，缺欄位給預設', () => {
  const p = toProject({
    ...base,
    executor_pool: [
      { id: 'codex-luna', kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 3 },
      { id: '', kind: 'claude', model: 'x', count: 1 }, // 沒有 id 的條目丟掉
      { id: 'agy-flash', kind: 'agy', model: 'gemini-3.8-flash-low', count: 99 }, // count 夾到 10
    ],
  })
  assert.deepEqual(p?.executor_pool, [
    { id: 'codex-luna', kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 3 },
    { id: 'agy-flash', kind: 'agy', model: 'gemini-3.8-flash-low', effort: null, context_tokens: null, count: 10 },
  ])
})

test('executor_pool：舊的物件形狀 {kind: n} 轉成 <kind>-legacy 條目（model 空字串＝任何模型）', () => {
  const p = toProject({ ...base, executor_pool: { codex: 2, claude: 1, nope: 4, grok: 0 } })
  assert.deepEqual(p?.executor_pool, [
    { id: 'codex-legacy', kind: 'codex', model: '', effort: null, context_tokens: null, count: 2 },
    { id: 'claude-legacy', kind: 'claude', model: '', effort: null, context_tokens: null, count: 1 },
  ])
})

test('executor_pool：缺欄位或型別錯誤＝空陣列', () => {
  assert.deepEqual(toProject({ ...base })?.executor_pool, [])
  assert.deepEqual(toProject({ ...base, executor_pool: 'bad' })?.executor_pool, [])
})
