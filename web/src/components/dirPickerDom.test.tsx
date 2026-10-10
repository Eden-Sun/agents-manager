/**
 * DirPicker 的鍵盤行為（真的掛進 happy-dom）：焦點在「取消」鈕時 Enter 屬於那顆按鈕（不能變成選目錄），
 * Esc 不管焦點在哪都是離開（輸入過濾字時先清過濾）、Enter／⌘Enter 在清單上是「進入」／「直接選」。
 */
import test, { after, afterEach, before } from 'node:test'
import { act } from 'react'
import assert from 'node:assert/strict'
import { click, fakeApi, keydown, mockApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { ApiError } from '../api/types'
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

test('daemon 說清單被截斷（資料夾太多）：畫面講出來，不是默默少一截', async () => {
  fakeApi((req) => (req.path.includes('/fs/dirs') ? { ...listing('/home/u'), truncated: true } : undefined))
  await mount(<DirPicker initial="/home/u" onPick={() => {}} onCancel={() => {}} />)
  await settle()
  assert.match(document.querySelector('[role=dialog]')!.textContent ?? '', /只列出前 2000 個/)
})

test('沒被截斷就不顯示那句', async () => {
  fakeApi((req) => (req.path.includes('/fs/dirs') ? { ...listing('/home/u'), truncated: false } : undefined))
  await mount(<DirPicker initial="/home/u" onPick={() => {}} onCancel={() => {}} />)
  await settle()
  assert.doesNotMatch(document.querySelector('[role=dialog]')!.textContent ?? '', /只列出前/)
})

// ───────── 新資料夾（issue #877） ─────────

async function openNew(host?: string, route?: (req: { method: string; path: string; body: unknown }) => unknown) {
  const requests = fakeApi((req) => {
    if (req.method === 'POST' && req.path.endsWith('/fs/dirs')) return route?.(req) ?? { path: `${(req.body as { parent: string }).parent}/${(req.body as { name: string }).name}`, name: (req.body as { name: string }).name, parent: (req.body as { parent: string }).parent }
    return req.path.includes('/fs/dirs') ? listing('/home/u') : undefined
  })
  const picked: string[] = []
  let cancelled = 0
  await mount(<DirPicker initial="/home/u" host={host} onPick={(p) => picked.push(p)} onCancel={() => cancelled++} />)
  await settle()
  const dialog = document.querySelector<HTMLElement>('[role=dialog]')!
  const button = (label: string) => [...dialog.querySelectorAll('button')].find((b) => b.textContent?.includes(label))!
  await click(button('新資料夾'))
  const input = dialog.querySelector<HTMLInputElement>('input[aria-label="新資料夾名稱"]')!
  const submit = async () => {
    await act(async () => dialog.querySelector('form[aria-label="新資料夾"]')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })))
    await settle()
  }
  return { requests, picked, cancelled: () => cancelled, dialog, button, input, submit }
}

test('新資料夾：在目前這一層建好就直接選取成 Project 路徑（本機請求不帶 host）', async () => {
  const d = await openNew()
  assert.match(d.dialog.textContent ?? '', /建在 \/home\/u/)
  await typeInto(d.input, '  my-new  ')
  await d.submit()
  const post = d.requests.find((r) => r.method === 'POST')!
  assert.deepEqual(post.body, { parent: '/home/u', name: 'my-new' })
  assert.deepEqual(d.picked, ['/home/u/my-new'], '建好直接選取，不是只回到清單')
})

test('新資料夾：遠端主機要帶 host', async () => {
  const d = await openNew('m4p')
  await typeInto(d.input, 'remote-proj')
  await d.submit()
  const post = d.requests.find((r) => r.method === 'POST')!
  assert.deepEqual(post.body, { parent: '/home/u', name: 'remote-proj', host: 'm4p' })
  assert.deepEqual(d.picked, ['/home/u/remote-proj'])
})

test('新資料夾：這一層已經有同名的、或名字不合法：當場提示，不送請求、不選取', async () => {
  const d = await openNew()
  for (const [name, hint] of [['alpha', /已經存在/], ['BETA', /已經存在/], ['a/b', /一次只建一層/], ['..', /不能是/], ['   ', /請輸入/]] as const) {
    await typeInto(d.input, name)
    await d.submit()
    assert.match(d.dialog.querySelector('[role=alert]')?.textContent ?? '', hint, name)
  }
  assert.equal(d.requests.filter((r) => r.method === 'POST').length, 0)
  assert.deepEqual(d.picked, [])
})

test('新資料夾：daemon 回 409 已存在，畫面顯示 daemon 的說明（不是機器碼），仍停在表單', async () => {
  mockApi({
    async request(method, path, body) {
      if (method === 'POST' && path === '/fs/dirs') {
        throw new ApiError(409, { error: 'conflict', reason: 'already_exists', message: '`taken` already exists in /home/u; pick another name or choose that folder' }, 'conflict')
      }
      return { path: '/home/u', parent: '/home', home: '/home/u', entries: [], truncated: false, body }
    },
    async upload() {
      return {}
    },
  })
  const picked: string[] = []
  await mount(<DirPicker initial="/home/u" onPick={(p) => picked.push(p)} onCancel={() => {}} />)
  await settle()
  const dialog = document.querySelector<HTMLElement>('[role=dialog]')!
  await click([...dialog.querySelectorAll('button')].find((b) => b.textContent?.includes('新資料夾'))!)
  await typeInto(dialog.querySelector<HTMLInputElement>('input[aria-label="新資料夾名稱"]')!, 'taken')
  await act(async () => dialog.querySelector('form[aria-label="新資料夾"]')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })))
  await settle()
  assert.match(dialog.querySelector('[role=alert]')?.textContent ?? '', /already exists in \/home\/u/)
  assert.deepEqual(picked, [])
  assert.ok(dialog.querySelector('form[aria-label="新資料夾"]'), '還在表單裡，可以改名再試')
})

test('新資料夾：Esc 先收起表單、不是離開整個對話框；再按一次 Esc 才離開', async () => {
  const d = await openNew()
  await keydown(d.input, 'Escape')
  assert.equal(d.dialog.querySelector('form[aria-label="新資料夾"]'), null)
  assert.equal(d.cancelled(), 0)
  await keydown(d.dialog, 'Escape')
  assert.equal(d.cancelled(), 1)
})

// ───────── 讀目錄中按 Esc（issue #960） ─────────

test('讀目錄中按 Esc：退出選擇器（onCancel），事件被 preventDefault，不會讓外層 Modal 關掉', async () => {
  // listDirs 永不回應 → busy 一直為 true
  mockApi({
    async request(method, path) {
      if (method === 'GET' && path.includes('/fs/dirs')) return new Promise(() => {})
      return {}
    },
    async upload() {
      return {}
    },
  })
  let cancelled = 0
  await mount(<DirPicker initial="/home/u" onPick={() => {}} onCancel={() => cancelled++} />)
  await settle()
  const filter = document.querySelector<HTMLInputElement>('[role=dialog] input.dirpicker-filter')!
  const e = await keydown(filter, 'Escape')
  assert.equal(cancelled, 1, 'busy 時 Esc 仍要離開選擇器')
  assert.equal(e.defaultPrevented, true, 'Esc 要 preventDefault，外層 Modal 才不會一起關')
})

test('讀目錄中按 ArrowDown：busy 仍擋其他鍵，不 preventDefault', async () => {
  mockApi({
    async request(method, path) {
      if (method === 'GET' && path.includes('/fs/dirs')) return new Promise(() => {})
      return {}
    },
    async upload() {
      return {}
    },
  })
  await mount(<DirPicker initial="/home/u" onPick={() => {}} onCancel={() => {}} />)
  await settle()
  const filter = document.querySelector<HTMLInputElement>('[role=dialog] input.dirpicker-filter')!
  const e = await keydown(filter, 'ArrowDown')
  assert.equal(e.defaultPrevented, false)
})

// ───────── 觸控裝置的焦點（issue #999） ─────────

/** 觸控裝置：`matchMedia` 對 `(pointer: coarse)` 回 true。 */
function asTouch(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('pointer: coarse'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

test('觸控：讀完一層不會把焦點搬進過濾框（不叫出軟鍵盤）', async () => {
  const restore = asTouch()
  try {
    fakeApi((req) => (req.path.includes('/fs/dirs') ? listing(new URL(`http://x${req.path}`).searchParams.get('path') || '/home/u') : undefined))
    await mount(<DirPicker initial="/home/u" onPick={() => {}} onCancel={() => {}} />)
    await settle()
    const dialog = document.querySelector<HTMLElement>('[role=dialog]')!
    const filter = dialog.querySelector<HTMLInputElement>('input.dirpicker-filter')!
    const cancel = [...dialog.querySelectorAll('button')].find((b) => b.textContent?.startsWith('取消'))!
    cancel.focus()
    // 進入 beta 再讀一次目錄：讀完不能把焦點拉回過濾框。
    await act(async () => dialog.querySelector('[role=option][data-idx="1"]')!.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })))
    await settle()
    assert.notEqual(document.activeElement, filter, '觸控讀完一層不能 focus 過濾框')
    assert.equal(document.activeElement, cancel)
  } finally {
    restore()
  }
})

test('觸控：點一列不攔 mousedown（不 preventDefault、焦點交還瀏覽器），選取照常', async () => {
  const restore = asTouch()
  try {
    fakeApi((req) => (req.path.includes('/fs/dirs') ? listing('/home/u') : undefined))
    await mount(<DirPicker initial="/home/u" onPick={() => {}} onCancel={() => {}} />)
    await settle()
    const row = document.querySelector<HTMLElement>('[role=dialog] [role=option]')!
    const ev = new MouseEvent('mousedown', { bubbles: true, cancelable: true })
    await act(async () => row.dispatchEvent(ev))
    assert.equal(ev.defaultPrevented, false, '觸控不能攔 mousedown，否則焦點留在過濾框、鍵盤不收')
    await click(row)
    assert.equal(row.getAttribute('aria-selected'), 'true')
  } finally {
    restore()
  }
})

test('桌機對照：點一列仍攔 mousedown、焦點留在過濾框（既有行為不回歸）', async () => {
  fakeApi((req) => (req.path.includes('/fs/dirs') ? listing('/home/u') : undefined))
  await mount(<DirPicker initial="/home/u" onPick={() => {}} onCancel={() => {}} />)
  await settle()
  const dialog = document.querySelector<HTMLElement>('[role=dialog]')!
  const filter = dialog.querySelector<HTMLInputElement>('input.dirpicker-filter')!
  ;[...dialog.querySelectorAll('button')].find((b) => b.textContent?.startsWith('取消'))!.focus()
  const row = dialog.querySelector<HTMLElement>('[role=option]')!
  const ev = new MouseEvent('mousedown', { bubbles: true, cancelable: true })
  await act(async () => row.dispatchEvent(ev))
  assert.equal(ev.defaultPrevented, true)
  assert.equal(document.activeElement, filter)
})

test('新資料夾：組字中的 Esc 不收表單、名字留著', async () => {
  const d = await open()
  await click(d.button('＋ 新資料夾'))
  const newNameInput = document.querySelector<HTMLInputElement>('form.dirpicker-new input')!
  await typeInto(newNameInput, '新資料')
  const ev = await keydown(newNameInput, 'Escape', { isComposing: true })
  assert.ok(document.querySelector('.dirpicker-new'), '新資料夾表單不能被收掉')
  assert.equal(newNameInput.value, '新資料')
  assert.equal(ev.defaultPrevented, true)
  assert.equal(d.cancelled(), 0)
  await keydown(newNameInput, 'Escape')
  assert.equal(document.querySelector('.dirpicker-new'), null, '一般的 Esc 收起表單')
})

test('過濾框：組字中的 Esc 不清過濾也不離開', async () => {
  const d = await open()
  const filter = document.querySelector<HTMLInputElement>('[role=dialog] input.dirpicker-filter')!
  await typeInto(filter, 'ab')
  await keydown(filter, 'Escape', { isComposing: true })
  assert.equal(filter.value, 'ab')
  assert.equal(d.cancelled(), 0)
  await typeInto(filter, '')
  await keydown(filter, 'Escape', { isComposing: true })
  assert.equal(d.cancelled(), 0)
})
