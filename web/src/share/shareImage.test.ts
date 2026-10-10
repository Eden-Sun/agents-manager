/** 分享頁圖片的純邏輯：哪些算圖、PNG 檔名與顯示名、SVG 外部資源偵測、大小推算、canvas 上限。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { displayName, imagesByMessage, isImageName, pngCanvasSize, pngName, svgExternalRefs, svgSize, svgWithSize } from './shareImage.ts'

test('圖片副檔名、PNG 檔名與顯示名', () => {
  for (const n of ['a.svg', 'B.PNG', 'c.jpg', 'd.jpeg', 'e.webp', 'f.gif']) assert.ok(isImageName(n), n)
  for (const n of ['a.html', 'svg', 'a.svg.txt', 'a.pdf']) assert.ok(!isImageName(n), n)
  assert.equal(pngName('星期日早安圖卡.svg'), '星期日早安圖卡.png')
  assert.equal(displayName('星期日早安圖卡.svg'), '星期日早安圖卡', '不顯示副檔名')
  assert.equal(displayName('photo.JPEG'), 'photo')
  assert.equal(displayName('.png'), '.png')
  assert.equal(displayName('報表.csv'), '報表.csv')
})

test('每則 bot 回覆下面畫哪些圖：那一回合做的圖、或回覆裡提到的', () => {
  const t = (min: number, sec = 0) => new Date(Date.UTC(2026, 9, 4, 8, min, sec)).toISOString()
  const msgs = [
    { id: 'u1', role: 'user', text: '做一張早安圖', created_at: t(0) },
    { id: 'b1', role: 'assistant', text: '做好了', created_at: t(1) },
    { id: 'u2', role: 'user', text: '再一張晚安的', created_at: t(2) },
    { id: 'b2', role: 'assistant', text: '好，晚安圖做好了；上一張 早安.svg 也還在', created_at: t(3) },
    { id: 'u3', role: 'user', text: '謝謝', created_at: t(4) },
  ]
  const f = (name: string, at: string | null) => ({ name, size: 1, modified_at: at })
  const out = imagesByMessage(msgs, [
    f('早安.svg', t(0, 40)), // 第一回合裡做的 → b1
    f('晚安.png', t(3, 1)), // 回完話才寫（時間在 b2 之後、u3 之前）→ 還是 b2
    f('筆記.txt', t(0, 50)), // 不是圖
    f('舊圖.png', null), // 沒時間：不靠時間掛
  ])
  assert.deepEqual(out.get('b1')?.map((x) => x.name), ['早安.svg'])
  assert.deepEqual(out.get('b2')?.map((x) => x.name), ['晚安.png', '早安.svg'], '提到的檔名也畫，同一則不重複')
  assert.equal(out.get('u1'), undefined, 'end user 的訊息不掛圖')
  // end user 剛送出、bot 在這一回合寫了圖但還沒回覆：先不掛在上一回合。
  const pending = imagesByMessage(msgs.slice(0, 3), [f('新圖.png', t(2, 30))])
  assert.equal(pending.get('b1'), undefined)
})

test('SVG 外部資源偵測：#id 與 data: 以外的引用都算', () => {
  const ok = `<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><defs><linearGradient id="g"/></defs>
    <rect fill="url(#g)"/><use href="#g"/><image href="data:image/png;base64,AAA"/><text>早安 ☀️</text></svg>`
  assert.equal(svgExternalRefs(ok), false)
  for (const bad of [
    '<svg><image href="https://evil.example/a.png"/></svg>',
    "<svg><image xlink:href='//cdn/a.png'/></svg>",
    '<svg><use href="other.svg#x"/></svg>',
    '<svg><rect style="fill:url(https://x/y)"/></svg>',
    '<svg><style>@import "https://fonts/x.css";</style></svg>',
    '<svg><foreignObject><div/></foreignObject></svg>',
  ]) {
    assert.equal(svgExternalRefs(bad), true, bad)
  }
})

test('SVG 大小：width/height 優先，其次 viewBox；補上大小只動根標籤', () => {
  assert.deepEqual(svgSize('<svg width="540" height="300px">'), { w: 540, h: 300 })
  assert.deepEqual(svgSize('<svg viewBox="0 0 1080 1350">'), { w: 1080, h: 1350 })
  assert.deepEqual(svgSize('<svg width="100%" viewBox="0,0,200,100">'), { w: 200, h: 100 })
  assert.deepEqual(svgSize('<svg width="400" viewBox="0 0 200 100">'), { w: 400, h: 200 })
  assert.equal(svgSize('<svg>'), null)
  const out = svgWithSize('<svg width="100%" stroke-width="2" viewBox="0 0 10 10"><rect width="5"/></svg>', { w: 10, h: 10 })
  assert.equal(out, '<svg width="10" height="10" stroke-width="2" viewBox="0 0 10 10"><rect width="5"/></svg>')
})

test('PNG 是 2x，但不超過 canvas 上限', () => {
  assert.deepEqual(pngCanvasSize(540, 540), { w: 1080, h: 1080 })
  const big = pngCanvasSize(6000, 4000)
  assert.ok(big.w <= 8192 && big.h <= 8192 && big.w * big.h <= 16_000_000, JSON.stringify(big))
})

test('回覆提到較長的檔名時，不把名字被它包住的另一張圖也掛上去', () => {
  const files = [
    { name: '早安.png', size: 1, modified_at: null },
    { name: '星期日早安.png', size: 1, modified_at: null },
    { name: '1.png', size: 1, modified_at: null },
    { name: '11.png', size: 1, modified_at: null },
  ]
  const msg = (id: string, text: string) => ({ id, role: 'assistant', text, created_at: '2026-10-04T00:00:00.000Z' })
  const names = (ms: ReturnType<typeof msg>[], id: string) => imagesByMessage(ms, files).get(id)?.map((x) => x.name)
  assert.deepEqual(names([msg('a', '做好了：星期日早安.png')], 'a'), ['星期日早安.png'], '修之前多一張 早安.png')
  assert.deepEqual(names([msg('b', '請看 11.png')], 'b'), ['11.png'])
  assert.deepEqual(names([msg('c', '早安.png 與 星期日早安.png 都好了')], 'c'), ['早安.png', '星期日早安.png'], '短的那個有一處是單獨出現')
  assert.deepEqual(names([msg('d', '1.png')], 'd'), ['1.png'])
  assert.equal(names([msg('e', '沒有提到圖')], 'e'), undefined)
})
