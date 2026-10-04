import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { BOT_KINDS, MODEL_OPTIONS } from '../api/types.ts'
import { KindIcon } from './KindTag.tsx'
import { KIND_DESC, KIND_LABEL } from './kindMeta.ts'

test('agy 是第四種 kind：列舉、標籤、說明、圖示、模型清單都有', () => {
  assert.ok(BOT_KINDS.includes('agy'))
  assert.equal(KIND_LABEL.agy, 'agy')
  assert.match(KIND_DESC.agy, /Antigravity/)
  assert.match(renderToStaticMarkup(<KindIcon kind="agy" />), /<svg/)
  assert.equal(MODEL_OPTIONS.agy[0], 'gemini-3.8-flash-medium', '第一個是預設')
  for (const slug of MODEL_OPTIONS.agy) assert.match(slug, /^(gemini|claude|gpt-oss)-.*(-high|-low|-medium|-thinking|-4-6)$/, '強度已經在 slug 裡')
})
