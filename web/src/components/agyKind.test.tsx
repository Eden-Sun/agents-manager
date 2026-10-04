import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { BOT_KINDS, MODEL_OPTIONS } from '../api/types.ts'
import { KindIcon } from './KindTag.tsx'
import { KIND_DESC, KIND_LABEL } from './kindMeta.ts'
import { contextLabel } from '../lib/contextLabel.ts'

test('agy 是第四種 kind：列舉、標籤、說明、圖示、模型清單都有', () => {
  assert.ok(BOT_KINDS.includes('agy'))
  assert.equal(KIND_LABEL.agy, 'agy')
  assert.match(KIND_DESC.agy, /Antigravity/)
  assert.match(renderToStaticMarkup(<KindIcon kind="agy" />), /<svg/)
  assert.equal(MODEL_OPTIONS.agy[0], 'gemini-3.8-flash-medium', '第一個是預設')
  for (const slug of MODEL_OPTIONS.agy) assert.match(slug, /^(gemini|claude|gpt-oss)-.*(-high|-low|-medium|-thinking|-4-6)$/, '強度已經在 slug 裡')
})

test('context 一行字：有百分比照舊；agy 只有 token 數時寫「約 Nk tokens」，不編百分比；都沒有就沒有這一行', () => {
  const st = (pct: number | null, tokens: number | null, size: number | null) => ({ context_used_pct: pct, context_used_tokens: tokens, context_size: size })
  assert.equal(contextLabel(st(40, 80_000, 200_000)), '40%（80k / 200k）')
  assert.equal(contextLabel(st(7, null, null)), '7%')
  assert.equal(contextLabel(st(null, 11_824, null)), '約 12k tokens')
  assert.equal(contextLabel(st(null, null, null)), null)
  assert.equal(contextLabel(st(null, 0, null)), null)
  assert.equal(contextLabel(null), null)
})
