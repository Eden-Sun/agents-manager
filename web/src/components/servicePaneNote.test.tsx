import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { ServicePaneNote } from './ServicePaneNote.tsx'
import type { CloseNeedsConfirm } from '../api'

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
