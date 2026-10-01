/**
 * 環境設定的「主機」面板（HostsPanel）：新增表單、重連、刪除。真的掛進 happy-dom，後端是 mock。
 * - 非法 ssh 目標（開頭 `-`、含空白）／非法 herdr_session：daemon 回 400（API.md §POST /api/hosts），畫面要看得到原因、
 *   表單內容留著讓人改、按鈕回到可按、清單不多出一台。
 * - 連不上的主機：200 但 `connected:false`，面板顯示失敗原因。
 * - 刪除要先確認；還有 Project 在用就不能按確認。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { sharedMock } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { HostsPanel } from './HostsPanel'

afterEach(async () => {
  await unmountAll()
  useStore.setState({ notices: [] }) // 上一個測試的通知不能讓下一個測試的 `until` 誤以為已經出現
})
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const mock = sharedMock
// mock 的 ssh／重連各要等 0.6–0.7 秒，加上輪詢，bun 預設 5 秒太緊。
const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const hostNames = () => useStore.getState().hosts.map((h) => h.name)
const input = (placeholder: string) => document.querySelector<HTMLInputElement>(`.hosts-panel form input[placeholder="${placeholder}"]`)!
const buttonByText = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent?.includes(text))!
const posts = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && r.path === '/api/hosts')
const errorNotices = () => useStore.getState().notices.filter((n) => n.kind === 'error').map((n) => n.text)

async function open() {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  await mount(<HostsPanel />)
  return requests
}

async function fillAndSubmit(name: string, ssh: string) {
  await typeInto(input('m4p'), name)
  await typeInto(input('m4p@100.112.229.82'), ssh)
  await click(buttonByText(document.body, '新增並連線'))
}

for (const [label, ssh] of [
  ['開頭是 - 的 ssh 目標（會被 ssh 當選項）', '-oProxyCommand=touch /tmp/pwned'],
  ['含空白的 ssh 目標', 'host name'],
] as const) {
  it(`新增主機：${label}被 400 擋下，畫面有原因、表單留著、清單不變`, async () => {
    const requests = await open()
    const before = hostNames()
    await fillAndSubmit('evil1', ssh)
    await until(() => errorNotices().length > 0, '錯誤通知')
    assert.equal(posts(requests).length, 1)
    assert.match(errorNotices().at(-1)!, /ssh/i, '通知講到 ssh 目標')
    assert.match(errorNotices().at(-1)!, /400/, '帶 HTTP 400')
    assert.deepEqual(hostNames(), before, '沒有多出主機')
    assert.equal(input('m4p').value, 'evil1', '名稱還在，讓人改')
    assert.equal(input('m4p@100.112.229.82').value, ssh, 'ssh 欄位還在，讓人改')
    assert.equal(buttonByText(document.body, '新增並連線').disabled, false, '按鈕回到可按')
    assert.equal(document.querySelector('.hosts-panel > .host-result'), null, '不是「連線失敗」那種結果條')
  })
}

it('新增主機：非法 herdr_session（含 /）也是 400', async () => {
  const requests = await open()
  await click(buttonByText(document.body, '進階'))
  const session = [...document.querySelectorAll<HTMLInputElement>('.hosts-panel form input')].find((i) => i.value === 'agents-manager')!
  await typeInto(session, '../../etc')
  await fillAndSubmit('evil2', 'build-box')
  await until(() => errorNotices().some((t) => /herdr_session/.test(t)), 'herdr_session 的錯誤通知')
  assert.equal(posts(requests).at(-1)!.body && (posts(requests).at(-1)!.body as { herdr_session: string }).herdr_session, '../../etc')
  assert.equal(hostNames().includes('evil2'), false)
})

it('新增主機：合法就出現在清單、表單清空、顯示就緒；刪除要先確認', async () => {
  const requests = await open()
  await fillAndSubmit('dom3ok', 'builder@10.1.2.3')
  await until(() => hostNames().includes('dom3ok'), '主機出現在 store')
  await until(() => document.querySelector('.hosts-panel > .host-result.ok') !== null, '就緒結果條')
  assert.match(document.querySelector('.hosts-panel > .host-result.ok')!.textContent ?? '', /dom3ok.*就緒/)
  assert.equal(input('m4p').value, '', '成功後表單清空')
  assert.ok([...document.querySelectorAll('.host-row .host-name')].some((e) => e.textContent?.includes('dom3ok')))

  const row = [...document.querySelectorAll('.host-row')].find((r) => r.textContent?.includes('dom3ok'))!
  await click(row.querySelector('button[aria-label="刪除主機"]')!)
  assert.equal(requests.some((r) => r.method === 'DELETE'), false, '按 ✕ 只是開確認框，還沒刪')
  const dialog = document.querySelector('[role=alertdialog]')!
  assert.ok(dialog)
  await click(buttonByText(dialog, '刪除主機'))
  await until(() => !hostNames().includes('dom3ok'), '刪掉了')
  assert.ok(requests.some((r) => r.method === 'DELETE' && r.path === '/api/hosts/dom3ok'))
})

it('新增主機：連不上（200 但 connected:false）顯示失敗原因；重連仍失敗也看得到', async () => {
  const requests = await open()
  await fillAndSubmit('dom3down', 'unreachable-box')
  await until(() => document.querySelector('.hosts-panel > .host-result.err') !== null, '失敗結果條')
  assert.match(document.querySelector('.hosts-panel > .host-result.err')!.textContent ?? '', /dom3down.*Operation timed out/)
  const row = [...document.querySelectorAll('.host-row')].find((r) => r.textContent?.includes('dom3down'))!
  assert.ok(row.querySelector('.host-err'), '清單列也標出未連線原因')
  await click(buttonByText(row, '重連'))
  await until(() => requests.some((r) => r.path === '/api/hosts/dom3down/reconnect'), '打了 reconnect')
  await until(() => useStore.getState().notices.some((n) => n.kind === 'error' && /dom3down 重連失敗/.test(n.text)), '重連失敗通知')
  await mock.request('DELETE', '/hosts/dom3down')
})

it('刪除主機：還有 Project 在用，確認鍵停用、不送 DELETE', async () => {
  const requests = await open()
  await mock.request('POST', '/hosts', { name: 'dom3used', ssh: 'me@dom3used' })
  await mock.request('POST', '/projects', { path: '/srv/dom3used', label: 'dom3used-proj', host: 'dom3used' })
  await useStore.getState().refreshState()
  await until(() => [...document.querySelectorAll('.host-row')].some((r) => r.textContent?.includes('dom3used')), '主機列出現')
  const row = [...document.querySelectorAll('.host-row')].find((r) => r.textContent?.includes('dom3used'))!
  await click(row.querySelector('button[aria-label="刪除主機"]')!)
  const dialog = document.querySelector('[role=alertdialog]')!
  assert.match(dialog.textContent ?? '', /仍有 1 個 Project/)
  const confirm = buttonByText(dialog, '刪除主機')
  assert.equal(confirm.disabled, true)
  await click(confirm)
  assert.equal(requests.some((r) => r.method === 'DELETE'), false)
})
