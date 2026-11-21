import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, settle, setupDom, teardownDom, unmountAll, until, type FakeRequest, fakeWebSocket } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

virtualMockTime()
const opened: string[] = []
afterEach(async () => {
  await unmountAll()
  for (const pane of opened.splice(0)) await mock.request('DELETE', `/hosts/local/shells/${encodeURIComponent(pane)}?confirm=true`).catch(() => {})
})
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})
beforeEach(() => {
  fakeWebSocket()
  localStorage.removeItem('am.shellKeySyncOff')
  localStorage.removeItem('am.shellKeySyncOn')
})

const mock = sharedMock

function asPhone(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

async function firstSnapshot(requests: FakeRequest[]) {
  await until(() => requests.some((r) => r.method === 'GET' && r.path.includes('/shells/')), '第一趟快照')
  await settle()
}

async function openShell() {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/shells', {})) as { pane_id: string; cwd: string }
  opened.push(shell.pane_id)
  await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
  await firstSnapshot(requests)
  return shell
}

const input = () => document.querySelector<HTMLElement>('.shell-input')
const syncBtn = () => [...document.querySelectorAll<HTMLButtonElement>('button.mini-btn')].find((b) => b.textContent?.startsWith('鍵盤直通'))!

test('手機：新開的 shell 沒有任何記錄，就有輸入框、鍵盤直通是關的', async () => {
  const restore = asPhone()
  try {
    await openShell()
    assert.ok(input(), '手機要有 .shell-input')
    assert.equal(syncBtn().getAttribute('aria-pressed'), 'false')
    assert.equal(syncBtn().textContent, '鍵盤直通（關）')
  } finally {
    restore()
  }
})

test('桌機：行為不變——沒有記錄＝直通開著、沒有輸入列', async () => {
  await openShell()
  assert.equal(input(), null)
  assert.equal(syncBtn().getAttribute('aria-pressed'), 'true')
})

test('手機明確打開直通：記進 am.shellKeySyncOn（並移出 off），再開同一顆 pane 仍是開的，提示改講手機沒有實體鍵盤', async () => {
  const restore = asPhone()
  try {
    const shell = await openShell()
    await click(syncBtn())
    await until(() => input() === null, '打開直通後輸入列收起')
    const target = `local/${shell.pane_id}`
    assert.deepEqual(JSON.parse(localStorage.getItem('am.shellKeySyncOn') ?? '[]'), [target])
    assert.deepEqual(JSON.parse(localStorage.getItem('am.shellKeySyncOff') ?? '[]'), [])
    const requests = mockApi(mock)
    await unmountAll()
    await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
    await firstSnapshot(requests)
    assert.equal(input(), null, '記著開：手機也照記錄')
    await act(async () => (document.activeElement as HTMLElement | null)?.blur())
    assert.match(document.querySelector('.shell-sync-note')?.textContent ?? '', /手機沒有實體鍵盤：關掉鍵盤直通改用下方輸入框/, '焦點不在終端上：手機不叫人「點一下畫面收鍵盤」')
    await click(syncBtn())
    await until(() => input() !== null, '關掉後輸入列回來')
    assert.deepEqual(JSON.parse(localStorage.getItem('am.shellKeySyncOn') ?? '[]'), [])
    assert.deepEqual(JSON.parse(localStorage.getItem('am.shellKeySyncOff') ?? '[]'), [target])
  } finally {
    restore()
  }
})

test('手機：送出中接著打的下一行不會被清掉（沒改字的送出照舊清空）', async () => {
  const restore = asPhone()
  const fetchBefore = globalThis.fetch
  let posted = 0
  let release!: () => void
  const gate = new Promise<void>((r) => (release = r))
  try {
    const shell = await openShell()
    const textPath = `/shells/${encodeURIComponent(shell.pane_id)}/text`
    const fetchNow = globalThis.fetch
    globalThis.fetch = (async (url: string, init?: { method?: string; body?: string }) => {
      if (init?.method === 'POST' && String(url).endsWith(textPath)) {
        await gate
        const res = await fetchNow(url, init as RequestInit)
        posted++
        return res
      }
      return fetchNow(url, init as RequestInit)
    }) as unknown as typeof fetch
    const cmdInput = () => document.querySelector<HTMLInputElement>('.shell-cmd')!
    const type = async (v: string) => {
      const el = cmdInput()
      await act(async () => {
        const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!
        set.call(el, v)
        el.dispatchEvent(new Event('input', { bubbles: true }))
      })
    }
    const enter = () => cmdInput().dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }))

    await type('ls')
    await act(async () => enter())
    await type('pwd')
    release()
    await until(() => posted === 1, '第一行送完')
    await settle()
    assert.equal(cmdInput().value, 'pwd', '送出期間打的下一行留著')

    await type('uptime')
    await act(async () => enter())
    await until(() => posted === 2, '第二行送完')
    await settle()
    assert.equal(cmdInput().value, '')
  } finally {
    release()
    globalThis.fetch = fetchBefore
    restore()
  }
})

test('手機：↑ 翻歷史再 ↓ 回到底，打到一半的那一行還在', async () => {
  localStorage.setItem('am.shellHistory', JSON.stringify({ local: ['ls'] }))
  const restore = asPhone()
  try {
    await openShell()
    const cmdInput = () => document.querySelector<HTMLInputElement>('.shell-cmd')!
    const setValue = async (v: string) => {
      await act(async () => {
        const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!
        set.call(cmdInput(), v)
        cmdInput().dispatchEvent(new Event('input', { bubbles: true }))
      })
    }
    const key = async (k: string) => {
      await act(async () => cmdInput().dispatchEvent(new KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true })))
    }

    await setValue('git st')
    await key('ArrowUp')
    assert.equal(cmdInput().value, 'ls', '↑ 翻到歷史')
    await key('ArrowDown')
    assert.equal(cmdInput().value, 'git st', '↓ 回到底：打到一半的字還在')

    await setValue('')
    await key('ArrowUp')
    await key('ArrowDown')
    assert.equal(cmdInput().value, '')
  } finally {
    localStorage.removeItem('am.shellHistory')
    restore()
  }
})
