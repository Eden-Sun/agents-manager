import test from 'node:test'
import assert from 'node:assert/strict'
import { createLatestOnly } from './latestOnly.ts'

test('後發的請求先回、先發的晚到：只有最後一張票算數', () => {
  const l = createLatestOnly()
  const first = l.begin()
  const second = l.begin()
  assert.equal(l.isCurrent(first), false)
  assert.equal(l.isCurrent(second), true)
})
