import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { BatchRow } from './UpdateAllBanner.tsx'
import type { RestartBatch } from '../api/types.ts'

const batch = (over: Partial<RestartBatch> = {}): RestartBatch => ({
  id: 'b1', total: 5, done: 2, current: 'C', ok: ['A', 'B'], failed: [], skipped: [], finished: false, ...over,
})

const render = (b: RestartBatch) => renderToStaticMarkup(<BatchRow batch={b} onClear={() => {}} />)

/**
 * **issue #492**：`bots_restart_done` 是唯一會把進度收尾的來源，而它有兩條收不到的路（批次中途 daemon
 * 重啟、客戶端落到全量 resync）。以前 ✕ 只在 `finished` 才 render，收不到時使用者只能重新整理分頁——
 * 而標題列那顆晶片同時被停用、又蓋住一鍵重啟的觸發鈕，等於連再按一次都不行。
 */
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

/**
 * **issue #492**：接回別人按出來的那一批走 `already_running`，回應只有 batch_id、沒有總數，
 * 要等 `bots_restart_progress` 補。以前 `total` 是 0 時 `pct` 直接當 100%，一接上就畫滿格、
 * 標題還寫「重啟中 0/0」——看起來像做完了。
 */
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
