import test from 'node:test'
import assert from 'node:assert/strict'
import { agmAttachmentBlock } from './agmAttachments.ts'

test('交給 AGM 帶著附件就不送，不靜靜丟掉附件（review3 c1 M7）', () => {
  assert.match(agmAttachmentBlock(true, 1) ?? '', /帶不了附件（1 個）/)
  assert.equal(agmAttachmentBlock(true, 0), null)
  assert.equal(agmAttachmentBlock(false, 3), null, '直接送給 bot 照常帶附件')
})
