/**
 * 沒裝 agy 的主機（使用者 2026-10-06）：額度 popover 的 agy 格寫「尚未安裝 agy，要自動安裝嗎？」，不開 shell 打 agy
 * （那只會得到 `zsh: command not found: agy`）；使用者按確認才打 `POST /api/hosts/{name}/agy/install`
 * （daemon 讀官方 manifest、驗 sha512，不跑官方 install.sh），裝好再接著開 shell 登入。新 bot 的 kind 選單／缺少 CLI 提示
 * 用的 `AgyInstallButton` 同樣先問。已經裝了就走原本的開 shell 登入。真的掛 `QuotaStrip` 進 happy-dom，後端是 mock。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'
import { AgyInstallButton } from './AgyInstall'

virtualMockTime()
/** 共用的 mock 也記著開過的 shell；留著會讓別的測試的 `openHostShell` 接回舊的、不再 POST 開新的。 */
async function closeShells() {
  const api = await import('../api')
  for (const sh of await api.fetchHostShells('local')) await api.closeHostShell('local', sh.pane_id, true)
}
afterEach(async () => {
  await unmountAll()
  await closeShells()
})
before(setupDom)

type MockAgy = { installed: boolean; path: string | null; version: string | null; logged_in: boolean | null }
const mockState = () => sharedMock as unknown as { localTools: { agy: MockAgy }; quota: Record<string, unknown> }
/** 共用的 mock 是行程單例（`bun test` 一個行程跑所有檔）：這支改了「沒裝 agy」，結束要放回去，免得別的 agy 測試看到沒裝。 */
const original = { agy: { ...mockState().localTools.agy }, quota: mockState().quota.agy }
after(async () => {
  mockState().localTools.agy = { ...original.agy }
  mockState().quota.agy = original.quota
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const btn = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent?.includes(text))
const agyRow = () => [...document.querySelectorAll('.quota-pop-row')].find((r) => r.querySelector('.quota-kind.agy'))!

/** 共用的 mock 是行程單例：每次開頭放回「本機沒裝 agy」。 */
function mockMissingAgy() {
  mockState().localTools.agy = { installed: false, path: null, version: null, logged_in: null }
  mockState().quota.agy = null
}

async function openAgyPopover() {
  mockMissingAgy()
  const requests = mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await mount(<QuotaStrip />)
  await until(() => document.querySelector('.quota-hp.agy') !== null, 'agy 那格畫出來')
  await click(document.querySelector('.quota-bars-open')!)
  await until(() => document.querySelector('.quota-pop') !== null, 'popover 開了')
  return requests
}

const installPosts = (requests: { method: string; path: string }[]) => requests.filter((r) => r.method === 'POST' && /\/hosts\/local\/agy\/install$/.test(r.path))
const shellPosts = (requests: { method: string; path: string }[]) => requests.filter((r) => r.method === 'POST' && /\/hosts\/local\/shells$/.test(r.path))

it('沒裝 agy：寫「尚未安裝，要自動安裝嗎？」；取消什麼都不打；確認才裝，裝好接著開 shell 登入', async () => {
  const requests = await openAgyPopover()
  assert.match(agyRow().textContent ?? '', /本機 尚未安裝 agy，要自動安裝嗎？/)
  assert.doesNotMatch(agyRow().textContent ?? '', /背景查詢中/)
  const go = btn(agyRow(), '安裝 agy 並登入')
  assert.ok(go, '沒裝的 agy 入口是「安裝並登入」，不是直接開 shell')
  assert.equal(btn(agyRow(), '開 shell 登入'), undefined)

  // 取消：不安裝、不開 shell。
  await click(go)
  await until(() => btn(document.body, '安裝並登入') !== undefined, '確認框出現')
  const dialog = document.body.textContent ?? ''
  assert.match(dialog, /尚未安裝 agy，要自動安裝嗎？/)
  assert.match(dialog, /sha512/)
  assert.match(dialog, /不跑官方 install\.sh/)
  assert.match(dialog, /裝好後會接著/)
  assert.equal(installPosts(requests).length, 0, '還沒確認不安裝')
  await click(btn(document.body, '取消')!)
  await act(async () => {})
  assert.equal(installPosts(requests).length + shellPosts(requests).length, 0, '取消後什麼都沒打')

  // 確認：先裝，裝好再開 shell 送 agy。
  await click(btn(agyRow(), '安裝 agy 並登入')!)
  await until(() => btn(document.body, '安裝並登入') !== undefined, '確認框再次出現')
  await click(btn(document.body, '安裝並登入')!)
  await until(() => installPosts(requests).length === 1, '送出 POST /hosts/local/agy/install')
  await until(() => shellPosts(requests).length === 1, '裝好後接著開 shell')
  const isText = (r: { method: string; path: string }) => r.method === 'POST' && /\/hosts\/local\/shells\/[^/]+\/text$/.test(r.path)
  await until(() => requests.some(isText), '往 shell 送字')
  assert.deepEqual(requests.find(isText)!.body, { text: 'AGY_CLI_DISABLE_AUTO_UPDATE=true agy', enter: true })
  const order = requests.filter((r) => r.method === 'POST').map((r) => r.path)
  assert.ok(order.findIndex((p) => /agy\/install$/.test(p)) < order.findIndex((p) => /\/shells$/.test(p)), '先安裝、後開 shell')
  const notes = useStore.getState().notices.map((n) => n.text).join('\n')
  assert.match(notes, /已安裝 agy 1\.3\.0/)
})

it('裝好之後（重讀快照）那格就是一般的未登入：開 shell 登入，沒有安裝入口', async () => {
  const requests = await openAgyPopover()
  await click(btn(agyRow(), '安裝 agy 並登入')!)
  await until(() => btn(document.body, '安裝並登入') !== undefined, '確認框出現')
  await click(btn(document.body, '安裝並登入')!)
  await until(() => installPosts(requests).length === 1, '送出安裝')
  await until(() => useStore.getState().busy['agy-install:local'] !== true, '安裝請求結束')
  // 這個 harness 沒有 WebSocket：daemon 的 host_changed 由重讀快照代替。
  await act(async () => {
    await useStore.getState().refreshState()
  })
  assert.equal(useStore.getState().localTools.agy.installed, true)
  await until(() => btn(agyRow(), '開 shell 登入') !== undefined, '出現開 shell 登入')
  assert.equal(btn(agyRow(), '安裝 agy 並登入'), undefined)
  assert.match(agyRow().textContent ?? '', /未登入/)
})

it('已經裝了 agy：沒有安裝入口', async () => {
  await openAgyPopover()
  await act(async () => {
    useStore.setState((st) => ({ localTools: { ...st.localTools, agy: { ...st.localTools.agy, installed: true, logged_in: false } } }))
  })
  assert.equal(btn(agyRow(), '安裝 agy 並登入'), undefined)
  assert.ok(btn(agyRow(), '開 shell 登入'))
})

it('新 bot 選單／缺少 CLI 提示用的安裝鈕：先問，確認才打 agy/install；已裝的主機不畫', async () => {
  mockMissingAgy()
  const requests = mockApi(sharedMock)
  await useStore.getState().refreshState()
  await mount(<AgyInstallButton host="local" small />)
  const small = btn(document.body, '安裝')!
  assert.ok(small)
  await click(small)
  await until(() => btn(document.body, '安裝 agy') !== undefined, '確認框出現')
  assert.match(document.body.textContent ?? '', /本機<\/strong>|本機 尚未安裝 agy，要自動安裝嗎？/)
  assert.equal(installPosts(requests).length, 0)
  await click(btn(document.body, '取消')!)
  assert.equal(installPosts(requests).length, 0, '取消不安裝')
  await click(btn(document.body, '安裝')!)
  await until(() => btn(document.body, '安裝 agy') !== undefined, '確認框再次出現')
  await click(btn(document.body, '安裝 agy')!)
  await until(() => installPosts(requests).length === 1, '確認後送出安裝')
  await act(async () => {
    await useStore.getState().refreshState()
  })
  await until(() => btn(document.body, '安裝') === undefined, '裝好就不畫安裝鈕')
})
