import assert from 'node:assert/strict'
import { test } from 'node:test'
import { PREVIEW_OFF, toPreview, toPreviewEvent } from './preview'

test('toPreview: 沒開過／壞資料都是 off', () => {
  assert.deepEqual(toPreview({ status: 'off' }), PREVIEW_OFF)
  assert.deepEqual(toPreview(null), PREVIEW_OFF)
  assert.equal(toPreview({ status: 'weird', port: 5180 }).status, 'off')
})

test('toPreviewEvent: running 沿用 dir，離開 failed 清錯誤，off 清 port', () => {
  const failed = { ...PREVIEW_OFF, status: 'failed' as const, port: 5180, dir: '/x/web', error: 'boom' }
  const starting = toPreviewEvent({ bot_id: 'b', status: 'starting', port: 5181 }, failed)
  assert.equal(starting.error, null)
  assert.equal(starting.dir, '/x/web')
  assert.equal(starting.port, 5181)
  const off = toPreviewEvent({ bot_id: 'b', status: 'off', port: null }, starting)
  assert.equal(off.port, null)
  assert.equal(off.status, 'off')
})

test('previewApiMissing: 404／405 是舊 daemon，其他錯誤不是', async () => {
  const { ApiError } = await import('./types')
  const { previewApiMissing } = await import('./preview')
  assert.equal(previewApiMissing(new ApiError(405, {}, 'x')), true)
  assert.equal(previewApiMissing(new ApiError(404, {}, 'x')), true)
  assert.equal(previewApiMissing(new ApiError(409, { reason: 'no_vite_config' }, 'x')), false)
  assert.equal(previewApiMissing(new Error('x')), false)
})
