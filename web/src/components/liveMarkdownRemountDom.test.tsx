/**
 * 串流中展開的半成品（#1236）：文字每 250ms 換一個新字串，`SafeMarkdown` 不能因此把整棵 Markdown 樹卸掉重掛。
 * 重掛會讓本機圖片每一幀重抓 `/local-image`、已載入的圖退回佔位，選取的文字也會消失。
 * 轉換失敗後的復原靠 `MarkdownBoundary` 的 `resetKey`，不靠把 boundary 整個 `key` 掉。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { MarkdownBoundary, SafeMarkdown } from './SafeMarkdown'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

const realFetch = globalThis.fetch
let localImageHits = 0

/** `/local-image` 回一張假圖（計次）；其餘路徑 404，`/session` 給 token。 */
function stubLocalImage() {
  localImageHits = 0
  globalThis.fetch = (async (input: string) => {
    const path = String(input)
    if (path.endsWith('/session')) return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (path.includes('/local-image')) {
      localImageHits += 1
      return new Response(new Blob(['png'], { type: 'image/png' }), { status: 200, headers: { 'content-type': 'image/png' } })
    }
    return new Response(JSON.stringify({}), { status: 404 })
  }) as unknown as typeof fetch
}

test('串流文字換了不重掛既有的圖片：本機圖只抓一次，圖片的 DOM 節點換字前後是同一個', async () => {
  stubLocalImage()
  try {
    let setText!: (t: string) => void
    function Live() {
      const [text, set] = useState('![](docs/a.png)\n\n第一段')
      setText = set
      return <SafeMarkdown text={text} botId="b1" cache={false} />
    }
    const root = await mount(<Live />)
    await until(() => root.querySelector('img.md-image') !== null, '本機圖片載入完成')
    const img = root.querySelector('img.md-image')!
    assert.equal(localImageHits, 1)

    await act(async () => setText('![](docs/a.png)\n\n第一段\n\n第二段'))
    await until(() => root.textContent?.includes('第二段') === true, '新的一段出現')
    await new Promise((r) => setTimeout(r, 50))

    assert.equal(localImageHits, 1, '換字不該再抓一次 /local-image')
    assert.equal(root.querySelector('img.md-image'), img, '圖片節點是同一個（沒有卸掉重掛）')
  } finally {
    globalThis.fetch = realFetch
  }
})

test('轉換失敗後，換了 resetKey 會復原成子元件內容（boundary 不靠 key 重掛）', async () => {
  const originalError = console.error
  console.error = () => {}
  let explode = true
  let setKey!: (k: string) => void
  function Child() {
    if (explode) throw new Error('boom')
    return <p>子元件內容</p>
  }
  function Host() {
    const [k, set] = useState('a')
    setKey = set
    return (
      <MarkdownBoundary resetKey={k} plain={<pre className="md-plain">純文字</pre>}>
        <Child />
      </MarkdownBoundary>
    )
  }
  try {
    const root = await mount(<Host />)
    assert.equal(root.querySelector('pre.md-plain')?.textContent, '純文字', '第一次 render 丟例外 → 退回純文字')

    explode = false
    await act(async () => setKey('b'))
    assert.equal(root.querySelector('p')?.textContent, '子元件內容', '換了 resetKey 就重試子元件')
  } finally {
    console.error = originalError
  }
})
