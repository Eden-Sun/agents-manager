import assert from 'node:assert/strict'
import { test } from 'node:test'
import { PREVIEW_COL_MIN, clampPreviewWidth, defaultPreviewWidth } from './previewLayout'

test('clampPreviewWidth: 下限 320、上限視窗 50%，放不下時下限優先', () => {
  assert.equal(clampPreviewWidth(100, 1600), PREVIEW_COL_MIN)
  assert.equal(clampPreviewWidth(5000, 1600), 800)
  assert.equal(clampPreviewWidth(700, 1600), 700)
  assert.equal(clampPreviewWidth(700, 400), PREVIEW_COL_MIN)
})

test('defaultPreviewWidth: 視窗的 40%，仍受夾限', () => {
  assert.equal(defaultPreviewWidth(1600), 640)
  assert.equal(defaultPreviewWidth(1100), 440)
})
