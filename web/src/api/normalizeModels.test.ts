import test from 'node:test'
import assert from 'node:assert/strict'
import { toModels } from './normalize'

// #1679：廠推薦（default_effort）與帳號記住的值（account_default_effort）是兩個欄位；舊 daemon 沒有後者 → null。
test('toModels 帶出 account_default_effort，缺欄位是 null', () => {
  const [a, b] = toModels({
    models: [
      { id: 'opus', default_effort: 'high', account_default_effort: 'low', efforts: ['low', 'high'] },
      { id: 'sonnet', default_effort: 'high', efforts: ['low', 'high'] },
    ],
  })
  assert.equal(a.default_effort, 'high')
  assert.equal(a.account_default_effort, 'low')
  assert.equal(b.default_effort, 'high')
  assert.equal(b.account_default_effort, null)
})
