import test from 'node:test'
import assert from 'node:assert/strict'
import { agoLabel } from './agoLabel.ts'

const now = Date.parse('2026-12-31T12:00:00.000Z')
const before = (ms: number) => new Date(now - ms).toISOString()

test('相對時間：不到一分鐘寫「剛剛」，不寫「0 分鐘前」', () => {
  assert.equal(agoLabel(before(0), now), '剛剛')
  assert.equal(agoLabel(before(20_000), now), '剛剛')
  assert.equal(agoLabel(before(59_999), now), '剛剛')
  // 時鐘比這台快（未來時間）也不寫負值。
  assert.equal(agoLabel(new Date(now + 5_000).toISOString(), now), '剛剛')
})

test('相對時間：分、時、天的邊界', () => {
  assert.equal(agoLabel(before(60_000), now), '1 分鐘前')
  assert.equal(agoLabel(before(59 * 60_000 + 59_000), now), '59 分鐘前')
  assert.equal(agoLabel(before(60 * 60_000), now), '1 小時前')
  assert.equal(agoLabel(before(23 * 3600_000 + 59 * 60_000), now), '23 小時前')
  assert.equal(agoLabel(before(24 * 3600_000), now), '24 小時前')
  assert.equal(agoLabel(before(48 * 3600_000), now), '2 天前')
})

test('相對時間讀不懂就是空字串', () => {
  assert.equal(agoLabel('not a date', now), '')
})
