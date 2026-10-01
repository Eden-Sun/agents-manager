/**
 * DirPicker 的鍵盤行為（真的掛進 happy-dom）：焦點在「取消」鈕時 Enter 屬於那顆按鈕（不能變成選目錄），
 * Esc 不管焦點在哪都是離開（輸入過濾字時先清過濾）、Enter／⌘Enter 在清單上是「進入」／「直接選」。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { DirPicker } from './DirPicker'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const listing = (path: string) => ({
  path,
  parent: path === '/home/u' ? '/home' : '/home/u',
  home: '/home/u',
  entries: [
    { name: 'alpha', path: `${path}/alpha`, git: false },
    { name: 'beta', path: `${path}/beta`, git: true },
  ],
})

async function open() {
  const requests = fakeApi((req) => (req.path.includes('/fs/dirs') ? listing(new URL(`http://x${req.path}`).searchParams.get('path') || '/home/u') : undefined))
  const picked: string[] = []
  let cancelled = 0
  await mount(<DirPicker initial="/home/u" onPick={(p) => picked.push(p)} onCancel={() => cancelled++} />)
  await settle()
  const dialog = document.querySelector<HTMLElement>('[role=dialog]')!
  const button = (label: string) => [...dialog.querySelectorAll('button')].find((b) => b.textContent?.startsWith(label))!
  return { requests, picked, cancelled: () => cancelled, dialog, button }
}

test('取消鈕上按 Enter：屬於那顆按鈕——容器不攔、不選目錄；點它才取消', async () => {
  const d = await open()
  const e = await keydown(d.button('取消'), 'Enter')
  assert.equal(e.defaultPrevented, false, '容器的 Enter 不能蓋過按鈕自己的')
  assert.deepEqual(d.picked, [], '按「取消」的 Enter 不能變成選擇目錄')
  assert.equal(d.cancelled(), 0)
  await click(d.button('取消'))
  assert.equal(d.cancelled(), 1)
  assert.deepEqual(d.picked, [])
})

test('Esc：焦點在取消鈕上也離開；輸入過濾字時先清過濾、不離開', async () => {
  const d = await open()
  await keydown(d.button('取消'), 'Escape')
  assert.equal(d.cancelled(), 1)

  const filter = d.dialog.querySelector<HTMLInputElement>('input[type=text], input:not([type])')
  assert.ok(filter, '要有過濾輸入框')
  await keydown(filter!, 'Escape')
  assert.equal(d.cancelled(), 2, '沒有過濾字時 Esc 就是離開')
})

test('清單上 ↓ 選到一個再 Enter＝進入那個目錄；⌘Enter＝直接選它', async () => {
  const d = await open()
  await keydown(d.dialog, 'ArrowDown')
  const before = d.requests.length
  await keydown(d.dialog, 'Enter')
  await settle()
  assert.ok(d.requests.length > before, 'Enter 要進入（再讀一次目錄）')
  assert.deepEqual(d.picked, [], 'Enter 不是選取')
  await keydown(d.dialog, 'ArrowDown')
  await keydown(d.dialog, 'Enter', { metaKey: true })
  assert.equal(d.picked.length, 1, '⌘Enter 直接選')
})
