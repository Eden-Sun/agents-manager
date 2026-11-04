import test from 'node:test'
import assert from 'node:assert/strict'
import { shareCreateFields } from './shareProfile'

test('新增 bot 的分享欄位：受限關 auto_approve、信任帶 confirm_trusted、沒選＝一般 bot', () => {
  const folder = { value: { kind: 'new' as const, name: 'exec-1' } }
  assert.deepEqual(shareCreateFields('restricted', folder), { share_profile: 'restricted', share_folder: folder.value, auto_approve: false })
  assert.deepEqual(shareCreateFields('trusted', folder), {
    share_profile: 'trusted',
    share_folder: folder.value,
    confirm_trusted: true,
    auto_approve: true,
  })
  assert.deepEqual(shareCreateFields(null, null), { auto_approve: true })
  assert.deepEqual(shareCreateFields('restricted', { error: 'x' }), { auto_approve: true })
})
