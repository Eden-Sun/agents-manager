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
