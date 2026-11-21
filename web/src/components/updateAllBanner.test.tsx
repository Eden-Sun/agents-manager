import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { BatchRow } from './UpdateAllBanner.tsx'
import type { RestartBatch } from '../api/types.ts'

const batch = (over: Partial<RestartBatch> = {}): RestartBatch => ({
  id: 'b1', total: 5, done: 2, current: 'C', ok: ['A', 'B'], failed: [], skipped: [], finished: false, ...over,
})

const render = (b: RestartBatch) => renderToStaticMarkup(<BatchRow batch={b} onClear={() => {}} />)

test('#492 還在跑的進度也要有收起鈕（收起不中斷批次）', () => {
  const html = render(batch())
  assert.match(html, /重啟中 2\/5/)
  assert.match(html, /aria-label="收起進度（不會中斷重啟）"/, `未完成時也要有 ✕：${html}`)
})

test('#492 跑完的摘要照舊有收起鈕，說法不一樣', () => {
  const html = render(batch({ finished: true, current: null }))
  assert.match(html, /重啟完成/)
  assert.match(html, /aria-label="收起這則摘要"/)
})

test('#492 接回別人那一批（總數未知）：不畫滿格、也不報 aria-valuenow', () => {
  const html = render(batch({ total: 0, done: 0, current: 'C', ok: [] }))
  assert.match(html, /重啟中 0 顆（總數未知）/)
  assert.match(html, /update-all-bar unknown/)
  assert.doesNotMatch(html, /width:\s*100%/, `總數未知時不能畫滿格：${html}`)
  assert.doesNotMatch(html, /aria-valuenow/, `不定量進度條不報 valuenow：${html}`)
})

test('#492 總數補上之後照常畫比例', () => {
  const html = render(batch({ total: 4, done: 1, ok: ['A'] }))
  assert.match(html, /重啟中 1\/4/)
  assert.match(html, /aria-valuenow="1"/)
  assert.doesNotMatch(html, /update-all-bar unknown/)
})
