/**
 * 訊息裡的圖片是內容，不是裝飾：沒寫替代文字（`![](x.png)`）時，圖片包在連結裡，alt 不能是空的——
 * 否則連結沒有任何可讀名稱，讀屏只唸「連結」。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { MarkdownImage } from './MarkdownImage'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('沒寫替代文字的遠端圖片：載入後連結裡的圖有 alt（檔名），連結有可讀名稱', async () => {
  const realFetch = globalThis.fetch
  globalThis.fetch = (async () => new Response(new Blob([new Uint8Array([137, 80, 78, 71])], { type: 'image/png' }), { headers: { 'content-type': 'image/png' } })) as typeof fetch
  try {
    await mount(<MarkdownImage botId="b1" src="https://img.example/cat.png" />)
    await until(() => document.querySelector('button.mini-btn') !== null, '載入圖片按鈕出現')
    await act(() => (document.querySelector('button.mini-btn') as HTMLButtonElement).click())
    await until(() => document.querySelector('a.md-image-link img') !== null, '圖片載入完成')
    const img = document.querySelector<HTMLImageElement>('a.md-image-link img')!
    assert.ok((img.getAttribute('alt') ?? '').trim() !== '', '替代文字不能是空的')
    assert.match(img.getAttribute('alt')!, /cat\.png/)
  } finally {
    globalThis.fetch = realFetch
  }
})
