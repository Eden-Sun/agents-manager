import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { ImageShelf } from './ImageShelf'
import { useShelf } from '../store/shelf'
import { resetStoreForTest } from '../store/store'

let realMatchMedia: typeof window.matchMedia
const realFetch = globalThis.fetch
const outboxFiles: { name: string; size: number; modified: number; remaining_secs: number }[] = []

function installMatchMedia(width: number) {
  window.matchMedia = ((query: string) => {
    const maxM = query.match(/max-width:\s*(\d+)px/)
    const minM = query.match(/min-width:\s*(\d+)px/)
    const matches = (maxM ? width <= Number(maxM[1]) : true) && (minM ? width >= Number(minM[1]) : true)
    return {
      matches,
      media: query,
      onchange: null,
      addEventListener() {},
      removeEventListener() {},
      addListener() {},
      removeListener() {},
      dispatchEvent() {
        return false
      },
    } as MediaQueryList
  }) as typeof window.matchMedia
}

before(async () => {
  await setupDom()
  realMatchMedia = window.matchMedia
  installMatchMedia(1600)
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ dir: '/tmp/outbox-a', reason: null, ttl_secs: 3600, kept: false, files: outboxFiles }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    )) as typeof fetch
})

afterEach(async () => {
  await unmountAll()
  localStorage.clear()
  useShelf.getState().clear()
  resetStoreForTest()
})

after(async () => {
  window.matchMedia = realMatchMedia
  globalThis.fetch = realFetch
  await teardownDom()
})

for (const width of [1440, 1180, 820, 390]) {
  test(`${width}px：展開時標題列有收合鈕，點了收合、重新載入後維持收合`, async () => {
    installMatchMedia(width)
    localStorage.setItem('am.shelf.open', '1')
    await mount(<ImageShelf />)
    assert.ok(document.querySelector('.shelf.open'))
    assert.equal(document.querySelector('.shelf-edge-fold'), null)
    const fold = document.querySelector<HTMLButtonElement>('.shelf-head .shelf-fold')
    assert.ok(fold, '標題列要有收合鈕')
    assert.equal(fold.getAttribute('aria-label'), '收合檔案暫存區')
    await act(async () => fold.click())
    await until(() => document.querySelector('.shelf.open') === null, '點了就收合')
    assert.ok(document.querySelector('.shelf-handle'))
    assert.equal(localStorage.getItem('am.shelf.open'), '0')
    await unmountAll()
    await mount(<ImageShelf />)
    assert.equal(document.querySelector('.shelf.open'), null, '重新載入後維持收合')
    const handle = document.querySelector<HTMLButtonElement>('.shelf-handle')!
    assert.equal(handle.getAttribute('aria-expanded'), 'false')
    await act(async () => handle.click())
    await until(() => document.querySelector('.shelf.open') !== null, '點 handle 再展開')
    assert.equal(localStorage.getItem('am.shelf.open'), '1')
  })
}
