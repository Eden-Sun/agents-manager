import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { ImageShelf } from './ImageShelf'
import { useShelf } from '../store/shelf'
import { resetStoreForTest, useStore } from '../store/store'

let realMatchMedia: typeof window.matchMedia
const realFetch = globalThis.fetch
let outboxFiles: { name: string; size: number; modified: number; remaining_secs: number }[] = []

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
  outboxFiles = []
  resetStoreForTest()
})

after(async () => {
  window.matchMedia = realMatchMedia
  globalThis.fetch = realFetch
  await teardownDom()
})

function fileDrag(type: string) {
  const ev = new Event(type, { bubbles: true })
  Object.defineProperty(ev, 'dataTransfer', { value: { types: ['Files'] } })
  return ev
}

test('桌面（1600px）空暫存區、沒有 outbox 檔案時預設收合成細邊條', async () => {
  await mount(<ImageShelf />)
  assert.equal(document.querySelector('.shelf.open'), null)
  assert.ok(document.querySelector('.shelf-handle'))
})

test('桌面使用者手動展開過就維持展開', async () => {
  localStorage.setItem('am.shelf.open', '1')
  await mount(<ImageShelf />)
  assert.ok(document.querySelector('.shelf.open'))
})

test('桌面拖檔進視窗時自動展開，拖完沒放東西就收回', async () => {
  await mount(<ImageShelf />)
  await act(async () => {
    window.dispatchEvent(fileDrag('dragenter'))
  })
  assert.ok(document.querySelector('.shelf.open'), '拖檔時展開')
  await act(async () => {
    window.dispatchEvent(fileDrag('dragend'))
  })
  assert.equal(document.querySelector('.shelf.open'), null, '拖完收回')
  assert.equal(localStorage.getItem('am.shelf.open'), null, '自動展開不寫入偏好')
})

test('桌面暫存區有檔案時預設展開', async () => {
  useShelf.getState().add([new File(['x'], 'a.txt', { type: 'text/plain' })])
  await mount(<ImageShelf />)
  assert.ok(document.querySelector('.shelf.open'))
})

test('桌面目前 bot 的 outbox 有檔案時預設展開', async () => {
  outboxFiles = [{ name: 'notes.pdf', size: 50_000, modified: 9, remaining_secs: 3500 }]
  useStore.setState({ selectedBotId: 'a', bots: [{ id: 'a', name: 'alpha', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null }] } as never)
  await mount(<ImageShelf />)
  await until(() => document.querySelector('.shelf.open') !== null, 'outbox 有檔案時展開')
  assert.ok(document.querySelector('.outbox-file'))
})
