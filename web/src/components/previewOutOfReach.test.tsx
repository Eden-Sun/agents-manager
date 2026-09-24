import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { OutOfReachNote } from './PreviewPanel.tsx'

/**
 * issue #527：`allow_lan` 關著時 dev server 釘在 loopback，手機／別台機器開的頁面連不到它。
 * 以前那裡是個空白 iframe，工具列卻寫著網址——要講出是哪一台、為什麼、怎麼辦。
 */
test('說明寫出這一頁的 host、loopback 與 allow_lan，不是一片空白', () => {
  const html = renderToStaticMarkup(<OutOfReachNote hostname="mac.tailnet.ts.net" https={false} />)
  assert.match(html, /mac\.tailnet\.ts\.net/)
  assert.match(html, /127\.0\.0\.1/)
  assert.match(html, /allow_lan/)
  assert.doesNotMatch(html, /混合內容/)
})

test('https 的頁面多講一句 mixed content（tailscale serve 那條路）', () => {
  assert.match(renderToStaticMarkup(<OutOfReachNote hostname="mac.tailnet.ts.net" https />), /混合內容/)
})
