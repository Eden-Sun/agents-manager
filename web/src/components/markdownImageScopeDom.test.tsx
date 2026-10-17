/**
 * Markdown 圖片換來源的生命週期（issue #1801）：換了 src 不能繼續畫上一張的 blob；失敗過一次後換成能載入的來源要恢復；
 * 晚到的舊請求不能污染新來源；遠端圖片換了 src 仍要重新手動授權。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useEffect, useState } from 'react'
import { act, click, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { MarkdownImage } from './MarkdownImage'

/** 還沒回的本機圖片請求：測試決定什麼時候、以什麼結果回。 */
interface LocalCall {
  path: string
  ok: (tag: string) => void
  fail: () => void
}

const localCalls: LocalCall[] = []
const revoked: string[] = []
let remoteN = 0

const realFetch = globalThis.fetch
const realCreate = URL.createObjectURL
const realRevoke = URL.revokeObjectURL

before(() => {
  setupDom()
  globalThis.fetch = ((input: string) => {
    const url = String(input)
    if (url.includes('/local-image')) {
      return new Promise((resolve, reject) => {
        localCalls.push({
          path: url,
          ok: (tag) => resolve({ ok: true, status: 200, blob: async () => ({ tag }) } as unknown as Response),
          fail: () => reject(new Error('gone')),
        })
      })
    }
    // 遠端：立刻回一張 PNG，讓測試只看 src 換了之後畫面怎麼變。
    return Promise.resolve(new Response(new Blob(['png'], { type: 'image/png' }), { status: 200, headers: { 'content-type': 'image/png' } }))
  }) as typeof fetch
  // 本機的 blob 帶 tag，直接當成 URL 的一部分，方便斷言「畫的是哪一張」。
  URL.createObjectURL = ((b: unknown) => {
    const tag = (b as { tag?: string }).tag
    return tag ? `blob:${tag}` : `blob:remote-${++remoteN}`
  }) as typeof URL.createObjectURL
  URL.revokeObjectURL = ((s: string) => {
    revoked.push(s)
  }) as typeof URL.revokeObjectURL
})

afterEach(async () => {
  await unmountAll()
  localCalls.length = 0
  revoked.length = 0
})

after(async () => {
  globalThis.fetch = realFetch
  URL.createObjectURL = realCreate
  URL.revokeObjectURL = realRevoke
  await teardownDom()
})

/** 一個可以從外面換 `src` 的宿主，模擬 bot 回覆的 Markdown 換了圖。 */
function host(initial: string) {
  let setSrc: (s: string) => void = () => {}
  function Host() {
    const [src, set] = useState(initial)
    useEffect(() => {
      setSrc = set
    }, [set])
    return <MarkdownImage botId="bot1" src={src} />
  }
  return { Host, change: (s: string) => act(() => setSrc(s)) }
}

const shownSrc = () => document.querySelector<HTMLImageElement>('img.md-image')?.getAttribute('src') ?? null
const localCall = (path: string) => {
  const c = localCalls.find((x) => x.path.includes(encodeURIComponent(path)))
  assert.ok(c, `應該有 ${path} 的請求`)
  return c
}

test('換來源後不畫上一張的圖（它已經被 revoke）', async () => {
  const { Host, change } = host('old.png')
  await mount(<Host />)
  await until(() => localCalls.length === 1, 'old.png 送出請求')
  localCall('old.png').ok('old')
  await until(() => shownSrc() === 'blob:old', '先畫出 old.png')

  await change('new.png')
  await settle(20)
  assert.equal(shownSrc(), null, '換成 new.png 還在請求時，不能繼續畫 blob:old')
  assert.ok(revoked.includes('blob:old'), 'blob:old 要被回收')

  localCall('new.png').ok('new')
  await until(() => shownSrc() === 'blob:new', '新來源載入後畫出 blob:new')
})

test('失敗過一次後換成能載入的來源，要恢復顯示圖', async () => {
  const { Host, change } = host('old.png')
  await mount(<Host />)
  await until(() => localCalls.length === 1, 'old.png 送出請求')
  localCall('old.png').fail()
  await until(() => document.querySelector('.md-image-missing') !== null, 'old.png 失敗後畫出缺圖')

  await change('good.png')
  localCall('good.png').ok('good')
  await until(() => shownSrc() === 'blob:good', '換成能成功的來源後要畫出圖，不能沿用上一次的失敗')
  assert.equal(document.querySelector('.md-image-missing'), null)
})

test('舊來源的請求晚到，不影響已經換成的新來源', async () => {
  const { Host, change } = host('old.png')
  await mount(<Host />)
  await until(() => localCalls.length === 1, 'old.png 送出請求')

  await change('new.png')
  await until(() => localCalls.length === 2, 'new.png 送出請求')
  localCall('new.png').ok('new')
  await until(() => shownSrc() === 'blob:new', '先畫出 new.png')

  // old.png 這時才回：它的結果不能蓋掉 new.png，而且要被回收。
  localCall('old.png').ok('late')
  await settle(20)
  assert.equal(shownSrc(), 'blob:new', '晚到的舊結果不能蓋掉新圖')
  assert.ok(revoked.includes('blob:late'), '晚到的舊 blob 要被回收')
})

test('遠端圖換了 src 要重新手動授權，不沿用上一張已載入的圖', async () => {
  const { Host, change } = host('https://a.example/one.png')
  await mount(<Host />)
  await click(document.querySelector<HTMLButtonElement>('button.mini-btn')!)
  await until(() => shownSrc() !== null, 'a.example 授權後畫出圖')
  const first = shownSrc()

  await change('https://b.example/two.png')
  await settle(20)
  assert.equal(document.querySelector('img.md-image'), null, '換到 b.example 之後不能還畫 a.example 的圖')
  assert.ok(first && revoked.includes(first), '上一張遠端 blob 要被回收')
  assert.ok(document.querySelector('.md-image-remote')?.textContent?.includes('b.example'), '佔位要改成新網域')
  assert.ok(document.querySelector('button.mini-btn'), '新網域要重新按「載入圖片」才抓')
})

test('A→B→A：A 的 blob 已 revoke，換回 A 時不能再畫它（review #1801）', async () => {
  const { Host, change } = host('old.png')
  await mount(<Host />)
  await until(() => localCalls.length === 1, 'old.png 送出請求')
  localCall('old.png').ok('old')
  await until(() => shownSrc() === 'blob:old', '先畫出 old.png')

  await change('new.png')
  await until(() => localCalls.length === 2, 'new.png 送出請求')
  await change('old.png')
  await until(() => localCalls.length === 3, '改回 old.png 再送出請求')

  await settle(20)
  assert.ok(revoked.includes('blob:old'), 'blob:old 已經被回收')
  assert.notEqual(shownSrc(), 'blob:old', '改回 old.png 時不能畫已 revoke 的 blob:old')
  assert.equal(shownSrc(), null, '還在載入，應該是載入中佔位')

  localCalls[2].ok('old2')
  await until(() => shownSrc() === 'blob:old2', '新的 old.png 結果到了才畫')
})

test('A 失敗→B→回 A：載入中不先畫上一次的缺圖', async () => {
  const { Host, change } = host('old.png')
  await mount(<Host />)
  await until(() => localCalls.length === 1, 'old.png 送出請求')
  localCall('old.png').fail()
  await until(() => document.querySelector('.md-image-missing') !== null, 'old.png 失敗後畫出缺圖')

  await change('new.png')
  await until(() => localCalls.length === 2, 'new.png 送出請求')
  await change('old.png')
  await until(() => localCalls.length === 3, '改回 old.png 再送出請求')

  await settle(20)
  assert.equal(document.querySelector('.md-image-missing'), null, '重新載入中不能先顯示上一次的缺圖')
  assert.ok(document.querySelector('.md-image-loading'), '應該是載入中佔位')
})
