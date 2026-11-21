import test from 'node:test'
import assert from 'node:assert/strict'
import { compactRefusal } from './compactRefusal.ts'

test('壓縮被擋的理由翻成一句人話；不認得的照原文', () => {
  assert.match(compactRefusal('agent_busy'), /忙/)
  assert.match(compactRefusal('turn_in_flight'), /回合/)
  assert.match(compactRefusal('not_running'), /沒有在跑/)
  assert.equal(compactRefusal('something_else'), 'something_else')
})
