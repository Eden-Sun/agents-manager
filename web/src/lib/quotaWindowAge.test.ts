import test from 'node:test'
import assert from 'node:assert/strict'
import { carriedOverAt } from './quotaWindowAge.ts'

const T1 = '2026-09-24T10:00:00.000Z'
const T2 = '2026-09-24T12:00:00.000Z'

test('#540：observed_at 比整筆的 updated_at 舊＝這一桶是沿用上一份的', () => {
  // statusline 這幾次只帶 7d，5h 被沿用：整筆是現在，那一桶還停在兩小時前。
  assert.equal(carriedOverAt({ observed_at: T1 }, T2), T1)
  // 這一次真的帶進來的窗，daemon 會把觀測時間蓋成 updated_at：不是沿用。
  assert.equal(carriedOverAt({ observed_at: T2 }, T2), null)
})

test('#540：沒有或解不開的時間戳一律不標——不知道就不要亂講', () => {
  assert.equal(carriedOverAt({ observed_at: null }, T2), null, '舊 daemon 沒有這一欄')
  assert.equal(carriedOverAt(null, T2), null)
  assert.equal(carriedOverAt(undefined, T2), null)
  assert.equal(carriedOverAt({ observed_at: 'not-a-date' }, T2), null)
  assert.equal(carriedOverAt({ observed_at: T1 }, 'not-a-date'), null)
  assert.equal(carriedOverAt({ observed_at: T1 }, null), null)
  // 比 updated_at 新（時鐘倒退之類）不算沿用。
  assert.equal(carriedOverAt({ observed_at: T2 }, T1), null)
})
