import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const btn = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent?.includes(text))
const agyRow = () => [...document.querySelectorAll('.quota-pop-row')].find((r) => r.querySelector('.quota-kind.agy'))!

function mockLoggedOutAgy() {
  const m = sharedMock as unknown as {
    localTools: { agy: { logged_in: boolean | null } }
    quota: Record<string, unknown>
  }
  m.localTools.agy.logged_in = false
  m.quota.agy = null
}

async function openAgyPopover() {
  mockLoggedOutAgy()
  const requests = mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await act(async () => {
    useStore.setState((st) => ({ localTools: { ...st.localTools, agy: { ...st.localTools.agy, logged_in: false } } }))
  })
  await mount(<QuotaStrip />)
  await until(() => document.querySelector('.quota-hp.agy') !== null, 'agy 那格畫出來')
  await click(document.querySelector('.quota-bars-open')!)
  await until(() => document.querySelector('.quota-pop') !== null, 'popover 開了')
  return requests
}

it('未登入：agy 那格有「開 shell 登入」鈕，說明 TUI 會引導登入；已登入就沒有', async () => {
  await openAgyPopover()
  assert.match(agyRow().textContent ?? '', /未登入/)
  const login = btn(agyRow(), '開 shell 登入')
  assert.ok(login, '未登入的 agy 要有登入入口')
  assert.equal(login.title, '在 本機 開一個 shell 並輸入 AGY_CLI_DISABLE_AUTO_UPDATE=true agy')
  assert.equal(btn(agyRow(), '登出 agy'), undefined, '未登入沒有登出鈕')
  await act(async () => {
    useStore.setState((st) => ({ localTools: { ...st.localTools, agy: { ...st.localTools.agy, logged_in: true } } }))
  })
  assert.equal(btn(agyRow(), '開 shell 登入'), undefined)
})

it('點「開 shell 登入」→ 確認框寫明授權網址與 /quit → 確認後開 shell、送 agy，登好後那格回到 5h／7d 額度', async () => {
  const requests = await openAgyPopover()
  await click(btn(agyRow(), '開 shell 登入')!)
  await until(() => btn(document.body, '開 shell 並送出') !== undefined, '確認框出現')
  const dialog = document.body.textContent ?? ''
  assert.match(dialog, /開 shell 登入 agy？/)
  assert.match(dialog, /授權網址/)
  assert.match(dialog, /授權碼貼回/)
  assert.match(dialog, /\/quit/)
  assert.equal(requests.filter((r) => r.method === 'POST' && /\/hosts\/local\/shells$/.test(r.path)).length, 0, '還沒確認不開 shell')

  await click(btn(document.body, '開 shell 並送出')!)
  await until(() => requests.some((r) => r.method === 'POST' && /\/hosts\/local\/shells$/.test(r.path)), '開了 shell')
  const isText = (r: { method: string; path: string }) => r.method === 'POST' && /\/hosts\/local\/shells\/[^/]+\/text$/.test(r.path)
  await until(() => requests.some(isText), '往 shell 送字')
  const text = requests.find(isText)!
  assert.deepEqual(text.body, { text: 'AGY_CLI_DISABLE_AUTO_UPDATE=true agy', enter: true })
  const view = useStore.getState().shellView
  assert.ok(view, '畫面切到那個 shell')
  assert.equal(useStore.getState().localTools.agy.logged_in, false, '還沒登好')

  const api = await import('../api')
  await api.sendHostShellText('local', view.paneId, '/quit', true)
  await act(async () => {
    await useStore.getState().refreshState()
    await useStore.getState().loadQuota()
  })
  await until(() => useStore.getState().localTools.agy.logged_in === true, '翻成已登入')
  await until(() => useStore.getState().quota.agy?.five_hour != null && useStore.getState().quota.agy?.seven_day != null, 'Gemini 的 5h／7d 額度回來')
  await act(async () => {})
  assert.doesNotMatch(agyRow().textContent ?? '', /未登入/)
  assert.equal(btn(agyRow(), '開 shell 登入'), undefined)
})

it('已經有一顆活著的 host shell：登入仍然新開一顆，指令不打進舊的那顆（#915）', async () => {
  const requests = await openAgyPopover()
  const api = await import('../api')
  const existing = await api.openHostShell('local')
  const isOpen = (r: { method: string; path: string }) => r.method === 'POST' && /\/hosts\/local\/shells$/.test(r.path)
  const before = requests.filter(isOpen).length
  const oldPath = `/shells/${encodeURIComponent(existing.pane_id)}/`
  assert.ok((await api.fetchHostShells('local')).some((s) => s.pane_id === existing.pane_id), '舊的那顆還活著')

  await click(btn(agyRow(), '開 shell 登入')!)
  await until(() => btn(document.body, '開 shell 並送出') !== undefined, '確認框出現')
  await click(btn(document.body, '開 shell 並送出')!)
  await until(() => requests.filter(isOpen).length === before + 1, '又新開了一顆 shell')
  const isText = (r: { method: string; path: string }) => r.method === 'POST' && /\/hosts\/local\/shells\/[^/]+\/text$/.test(r.path)
  await until(() => requests.some((r) => isText(r) && !r.path.includes(oldPath)), '字打進新的那顆')
  assert.ok(!requests.some((r) => isText(r) && r.path.includes(oldPath)), '舊 shell 沒收到任何字')
  const view = useStore.getState().shellView
  assert.ok(view && view.paneId !== existing.pane_id, 'shellView 是新的那顆')
  for (const sh of await api.fetchHostShells('local')) await api.closeHostShell('local', sh.pane_id, true)
})

