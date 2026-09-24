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
