import test from 'node:test'
import assert from 'node:assert/strict'
import { relayFromName } from './relayFromName.ts'

test('活著的來源用清單的名字', () => {
  assert.equal(relayFromName({ fromId: 'live-1', liveName: 'alfa', savedName: 'alfa', deleted: false }), 'alfa')
})

test('daemon 補的名字：活著照樣顯示，已收掉才標', () => {
  assert.equal(relayFromName({ fromId: 'saved-1', liveName: '', savedName: 'beta', deleted: false }), 'beta')
  assert.equal(relayFromName({ fromId: 'saved-2', liveName: '', savedName: 'lx1752', deleted: true }), 'lx1752（已收掉）')
})

test('先有名字 → bot 從清單消失 → 仍顯示「名字（已收掉）」而不是 ID', () => {
  const id = '01M4VV32AKTE3AFYMFZP30KDA9'
  // 收掉前：SSE 進來、清單找得到，順手記住
  assert.equal(relayFromName({ fromId: id, liveName: 'lx1752' }), 'lx1752')
  // 收掉後：清單沒有、daemon 也沒補名字（SSE 訊息沒有 relay_from_name）
  assert.equal(relayFromName({ fromId: id, liveName: '', savedName: undefined }), 'lx1752（已收掉）')
})

test('從來沒解析到名字、清單也沒有：回空字串，呼叫端退回 ID', () => {
  assert.equal(relayFromName({ fromId: 'never-seen', liveName: '', savedName: null }), '')
})
