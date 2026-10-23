import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { AgyUpdateChip } from './AgyUpdateChip.tsx'
import { agyUpdateTitle } from '../lib/agyUpdate.ts'

test('agy 都追上了（沒有快照）就不畫', () => {
  assert.equal(renderToStaticMarkup(<AgyUpdateChip />), '')
})

test('確認框標題跟設定頁「更新檢查」同一句（#1921 重用同一個框）', () => {
  assert.equal(agyUpdateTitle('1.4.6'), '更新 Antigravity（agy）CLI 到 1.4.6？')
})
