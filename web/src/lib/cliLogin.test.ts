/**
 * #915：沒綁身分的 CLI 登入要在**新開**的 host shell 打指令，不能接回最近一個還活著的 shell
 * （那顆的前景可能是 vim／sudo／手動開的 claude）。綁了身分的走 daemon 的 identity login。後端是 mock。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { startCliLogin } from './cliLogin'

virtualMockTime()
/** 共用的 mock 是行程單例、shell 上限 8 顆：開過的收掉，別讓後面的測試撞 `too_many_shells`。 */
async function closeLocalShells() {
  const api = await import('../api')
  for (const sh of await api.fetchHostShells('local')) await api.closeHostShell('local', sh.pane_id, true)
}
afterEach(async () => {
  await unmountAll()
  await closeLocalShells()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

function seedBot(identity: string | null) {
  useStore.setState({
    projects: [{ id: 'p-cli', host: 'local' }] as never,
    bots: [{ id: 'b-cli', project_id: 'p-cli', kind: 'claude', identity }] as never,
  })
}

it('沒綁身分：新開一顆 shell 打登入指令，不查也不接回既有的 shell', async () => {
  const requests = mockApi(sharedMock)
  await closeLocalShells() // 前面的測試可能已把共用 mock 的 shell 開到上限
  seedBot(null)
  const api = await import('../api')
  const existing = await api.openHostShell('local')
  requests.length = 0

  assert.equal(await startCliLogin('b-cli'), true)

  const opens = requests.filter((r) => r.method === 'POST' && /\/hosts\/local\/shells$/.test(r.path))
  assert.equal(opens.length, 1, 'api.openHostShell 恰一次')
  assert.equal(requests.filter((r) => r.method === 'GET' && /\/hosts\/local\/shells$/.test(r.path)).length, 0, '不列既有 shell（fetchHostShells）')
  const text = requests.find((r) => r.method === 'POST' && /\/hosts\/local\/shells\/[^/]+\/text$/.test(r.path))
  assert.ok(text, '往 shell 送字')
  assert.ok(!text.path.includes(`/shells/${encodeURIComponent(existing.pane_id)}/`), '指令不能打進既有那顆 shell')
  const view = useStore.getState().shellView
  assert.ok(view && view.paneId !== existing.pane_id, '畫面切到新的那顆')
  assert.ok(text.path.includes(`/shells/${encodeURIComponent(view.paneId)}/text`), `字打進畫面上的那顆：${text.path} vs ${view.paneId}`)
})

it('綁了身分：走 daemon 的 identity login，不開 host shell', async () => {
  const requests = mockApi(sharedMock)
  seedBot('work')
  requests.length = 0

  await startCliLogin('b-cli')

  assert.ok(requests.some((r) => r.method === 'POST' && /\/hosts\/local\/identities\/work\/login$/.test(r.path)), '呼叫 identity login')
  assert.equal(requests.filter((r) => r.method === 'POST' && /\/hosts\/local\/shells$/.test(r.path)).length, 0, '不另外開 host shell')
  assert.equal(requests.filter((r) => /\/hosts\/local\/shells$/.test(r.path) && r.method === 'GET').length, 0)
})
