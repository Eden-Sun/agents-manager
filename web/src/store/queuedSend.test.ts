import test from 'node:test'
import assert from 'node:assert/strict'
import { restoreQueued } from './queuedSend.ts'

const pending = { text: '排隊那句', attachments: ['a1', 'a2'] }

test('槽位空著：原樣排回佇列，附件 id 一個不少', () => {
  const r = restoreQueued({ queuedSends: {}, drafts: { 'bot:b1': '打到一半' } }, 'b1', pending)
  assert.deepEqual(r.patch, { queuedSends: { b1: pending } })
  assert.equal(r.droppedAttachments, 0)
})

test('槽位被新的一則佔走：不蓋掉新的，舊文字接回輸入框最前面', () => {
  const newer = { text: '新排的', attachments: [] }
  const r = restoreQueued({ queuedSends: { b1: newer }, drafts: { 'bot:b1': '打到一半' } }, 'b1', pending)
  assert.equal(r.patch.queuedSends, undefined)
  assert.deepEqual(r.patch.drafts, { 'bot:b1': '排隊那句\n打到一半' })
  assert.equal(r.droppedAttachments, 2)
})

test('輸入框是空的就只放舊文字，不多一個換行', () => {
  const r = restoreQueued({ queuedSends: { b1: { text: 'x', attachments: [] } }, drafts: {} }, 'b1', pending)
  assert.deepEqual(r.patch.drafts, { 'bot:b1': '排隊那句' })
})

test('別顆 bot 的佇列與草稿不受影響', () => {
  const s = { queuedSends: { b2: { text: 'z', attachments: [] } }, drafts: { 'bot:b2': 'zz' } }
  const r = restoreQueued(s, 'b1', pending)
  assert.deepEqual(r.patch.queuedSends, { b2: s.queuedSends.b2, b1: pending })
  assert.equal(r.patch.drafts, undefined)
})
