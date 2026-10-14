import test from 'node:test'
import assert from 'node:assert/strict'
import { relayFromName } from './relayFromName.ts'

test('活著的來源用清單的名字', () => {
  assert.equal(relayFromName({ liveName: 'alfa', savedName: 'alfa', deleted: false }), 'alfa')
})

test('已收掉的來源（清單找不到）用 daemon 補的名字並標已收掉', () => {
  assert.equal(relayFromName({ liveName: '', savedName: 'lx1752', deleted: true }), 'lx1752（已收掉）')
})

test('沒有任何名字才回空字串，呼叫端退回 ID', () => {
  assert.equal(relayFromName({ liveName: '', savedName: null }), '')
  assert.equal(relayFromName({ liveName: '' }), '')
})
