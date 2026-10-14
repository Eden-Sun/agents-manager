import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { BOT_KINDS, MODEL_OPTIONS, agyModelAllowed, canonicalModel } from '../api/types.ts'
import { KindIcon } from './KindTag.tsx'
import { KIND_DESC, KIND_LABEL } from './kindMeta.ts'
import { contextLabel } from '../lib/contextLabel.ts'

test('agy 收 Gemini 3.8 含以上（2026-10-14）：3.7 以下、非 Gemini 不收，4／4-argon／3.10 收', () => {
  for (const ok of ['gemini-3.8-flash-medium', 'gemini-3.8-flash-high', 'gemini-4-argon', 'gemini-4.1-argon-high', 'gemini-3.10-flash-low']) assert.equal(agyModelAllowed(ok), true, ok)
  for (const no of ['gemini-3.7-flash-high', 'gemini-3.6-flash-low', 'gemini-3.1-pro-high', 'gemini-3-flash', 'gemini-argon', 'claude-sonnet-4-6', 'claude-opus-4-6-thinking', 'gpt-oss-120b-medium']) {
    assert.equal(agyModelAllowed(no), false, no)
    assert.equal(canonicalModel('agy', no), 'gemini-3.8-flash-medium', no)
  }
  assert.equal(canonicalModel('agy', 'gemini-4-argon'), 'gemini-4-argon', '收的原樣')
  assert.equal(canonicalModel('claude', 'claude-sonnet-4-6'), 'claude-sonnet-4-6', '只對 agy')
})

test('agy 是第四種 kind：列舉、標籤、說明、圖示、後備模型清單都有', () => {
  assert.ok(BOT_KINDS.includes('agy'))
  assert.equal(KIND_LABEL.agy, 'agy')
  assert.match(KIND_DESC.agy, /Antigravity/)
  assert.match(renderToStaticMarkup(<KindIcon kind="agy" />), /<svg/)
  assert.deepEqual(MODEL_OPTIONS.agy, ['gemini-3.8-flash-medium', 'gemini-3.8-flash-high', 'gemini-3.8-flash-low'], '後備清單：3.8 Flash 三檔，第一個是預設')
  for (const slug of MODEL_OPTIONS.agy) assert.ok(agyModelAllowed(slug), slug)
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
