/**
 * #931：手機（≤640px）開 host shell 要有輸入框可打字——鍵盤直通預設關（手機點 `<pre>` 叫不出軟鍵盤），
 * 桌機行為不變（沒有記錄＝直通開著、沒有輸入列）。真的掛 `HostShellPanel` 進 happy-dom，後端是 `MockTransport`。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

virtualMockTime()
// 共用的 mock 一個主機最多 8 個 shell，而且跨測試檔累積：自己開的自己關，不然整套跑下來後面的檔案會 409 too_many_shells。
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
  localStorage.removeItem('am.shellKeySyncOff')
  localStorage.removeItem('am.shellKeySyncOn')
})

const mock = sharedMock

/** 手機寬度：`matchMedia` 對 640px 斷點回 true。 */
function asPhone(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

async function openShell() {
  mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/shells', {})) as { pane_id: string; cwd: string }
  opened.push(shell.pane_id)
  await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
  await settle(300)
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
    // 同一顆 pane 重新掛上來：記著開就是開。
    await unmountAll()
    await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
    await settle(300)
    assert.equal(input(), null, '記著開：手機也照記錄')
    await act(async () => (document.activeElement as HTMLElement | null)?.blur())
    assert.match(document.querySelector('.shell-sync-note')?.textContent ?? '', /手機沒有實體鍵盤：關掉鍵盤直通改用下方輸入框/, '焦點不在終端上：手機不叫人「點一下畫面收鍵盤」')
    // 再關掉：on 移除、off 加入。
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
  let release!: () => void
  const gate = new Promise<void>((r) => (release = r))
  try {
    const shell = await openShell()
    // 送出的 POST 先等 gate 再真的送出：這段時間輸入框沒有停用，可以接著打字。
    const textPath = `/shells/${encodeURIComponent(shell.pane_id)}/text`
    const fetchNow = globalThis.fetch
    globalThis.fetch = (async (url: string, init?: { method?: string; body?: string }) => {
      if (init?.method === 'POST' && String(url).endsWith(textPath)) await gate
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
    await settle(300)
    assert.equal(cmdInput().value, 'pwd', '送出期間打的下一行留著')

    // 對照：沒有改字的送出，送完框是空的。
    await type('uptime')
    await act(async () => enter())
    await settle(300)
    assert.equal(cmdInput().value, '')
  } finally {
    release()
    globalThis.fetch = fetchBefore
    restore()
  }
})

/** #1128：↑ 翻歷史會換掉輸入框，打到一半的那一行要在 ↓ 回到底時還回來。 */
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

    // 對照：空的輸入框 ↑ 再 ↓ 是空的，不會把前一次的 pending 帶回來。
    await setValue('')
    await key('ArrowUp')
    await key('ArrowDown')
    assert.equal(cmdInput().value, '')
  } finally {
    localStorage.removeItem('am.shellHistory')
    restore()
  }
})
