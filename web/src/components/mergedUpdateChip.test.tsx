import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { MergedUpdateChip } from './MergedUpdateChip.tsx'
import { mergedUpdateLabels } from '../lib/mergedUpdateLabels.ts'

/**
 * 手機 390px：兩顆並排時長名字只剩 ▾（實測 16px）。合成的這顆只留圖示（不畫數字），
 * 兩件事的說明寫在 aria-label／tooltip 裡，點開才分兩項。
 */
test('合成那顆只留圖示、是選單鈕', () => {
  const html = renderToStaticMarkup(<MergedUpdateChip />)
  assert.match(html, /class="quota-update install merged"/)
  assert.doesNotMatch(html, /quota-update-n/, `手機合成那顆不畫數字：${html}`)
  assert.match(html, /aria-haspopup="menu"/)
})

const base = { batch: null, cli: null, readyCount: 1, busyCount: 0, installCount: 2, to: '0.157.0' }

test('兩項選單各講自己那件事，aria-label 兩件都寫', () => {
  const l = mergedUpdateLabels(base)
  assert.equal(l.restartItem, '重啟 1 顆閒置的 Bot 套用更新')
  assert.equal(l.codexItem, '安裝 codex 0.157.0 並重啟（2 顆還沒裝）')
  assert.match(l.label, /重啟 1 顆.*安裝 codex 0\.157\.0/)
})

test('批次在跑、codex 在裝、全在忙時的文案', () => {
  const batch = { id: 'b', total: 3, done: 1, current: null, ok: ['a'], failed: [], skipped: [], finished: false }
  assert.equal(mergedUpdateLabels({ ...base, batch }).restartItem, '重啟中 1/3（收起，不會中斷）')
  assert.match(mergedUpdateLabels({ ...base, batch: { ...batch, finished: true } }).restartItem, /重啟完成 · 成功 1 顆/)
  const cli = { id: 'u', host: 'local', kind: 'codex', phase: 'installing' as const, from: null, to: null }
  assert.equal(mergedUpdateLabels({ ...base, cli }).codexItem, 'local 的 codex 安裝中…')
  assert.equal(mergedUpdateLabels({ ...base, readyCount: 0, busyCount: 2 }).restartItem, '2 顆有更新但在忙，閒下來再按')
})
