import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { ServicePaneNote } from './ServicePaneNote.tsx'
import type { CloseNeedsConfirm } from '../api'

/**
 * 結束 shell 被 daemon 擋下（服務 pane／讀不到狀態）要人再確認：環境設定的主機列與 shell 面板用同一段說明，
 * 不能各寫一份——主機列的 ✕ 以前把這個回傳值整個丟掉，點了什麼都沒發生也沒有任何提示。
 */
test('講出 port 與前景指令', () => {
  const needs = { pane: { pane_id: 'p1', listen_ports: [3010, 5173], foreground: 'vite' }, unverified: false } as unknown as CloseNeedsConfirm
  const html = renderToStaticMarkup(<ServicePaneNote needs={needs} />)
  assert.match(html, /3010、5173/)
  assert.match(html, /vite/)
  assert.match(html, /listen/)
})

test('讀不到 pane 在跑什麼時講明無法確認', () => {
  const html = renderToStaticMarkup(<ServicePaneNote needs={{ pane: null, unverified: true }} />)
  assert.match(html, /讀不到/)
  assert.doesNotMatch(html, /listen/)
})
