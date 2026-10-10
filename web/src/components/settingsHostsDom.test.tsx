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
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { HostsPanel } from './HostsPanel'
import * as api from '../api'

virtualMockTime()
afterEach(async () => {
  await unmountAll()
  useStore.setState({ notices: [] }) // 上一個測試的通知不能讓下一個測試的 `until` 誤以為已經出現
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mock = sharedMock
// mock 的 ssh／重連各要等 0.6–0.7 秒，加上輪詢，bun 預設 5 秒太緊。
const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const hostNames = () => useStore.getState().hosts.map((h) => h.name)
const input = (placeholder: string) => document.querySelector<HTMLInputElement>(`.hosts-panel form input[placeholder="${placeholder}"]`)!
const buttonByText = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent?.includes(text))!
const posts = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && r.path === '/api/hosts')

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

// 這兩種 daemon 會 400；前端現在先擋（`lib/hostForm`，跟 `config::host_target_problem` 同一條規則）：說原因、不送請求、表單留著。
for (const [label, ssh] of [
  ['開頭是 - 的 ssh 目標（會被 ssh 當選項）', '-oProxyCommand=touch /tmp/pwned'],
  ['含空白的 ssh 目標', 'host name'],
] as const) {
  it(`新增主機：${label}前端先擋，畫面有原因、不送請求、表單留著、清單不變`, async () => {
    const requests = await open()
    const before = hostNames()
    await typeInto(input('m4p'), 'evil1')
    await typeInto(input('m4p@100.112.229.82'), ssh)
    assert.match(document.querySelector('.hosts-panel form')!.textContent ?? '', /ssh 目標要是主機/, '畫面說原因')
    assert.equal(buttonByText(document.body, '新增並連線').disabled, true, '按鈕鎖住')
    assert.equal(posts(requests).length, 0, '一個請求都沒送')
    assert.deepEqual(hostNames(), before, '沒有多出主機')
    assert.equal(input('m4p').value, 'evil1', '名稱還在，讓人改')
    assert.equal(input('m4p@100.112.229.82').value, ssh, 'ssh 欄位還在，讓人改')
  })
}

it('新增主機：非法 herdr_session（含 /）、打錯的 port 前端先擋', async () => {
  const requests = await open()
  await click(buttonByText(document.body, '進階'))
  const inputs = () => [...document.querySelectorAll<HTMLInputElement>('.hosts-panel form input')]
  await typeInto(inputs().find((i) => i.value === 'agents-manager')!, '../../etc')
  await typeInto(input('m4p'), 'evil2')
  await typeInto(input('m4p@100.112.229.82'), 'build-box')
  assert.equal(buttonByText(document.body, '新增並連線').disabled, true)
  assert.match(document.querySelector('.hosts-panel form')!.textContent ?? '', /herdr_session 要是/)
  await typeInto(inputs().find((i) => i.value === '../../etc')!, 'agents-manager')
  await typeInto(inputs().find((i) => i.value === '22')!, '2222x')
  assert.equal(buttonByText(document.body, '新增並連線').disabled, true, '「2222x」以前悄悄變成 22')
  assert.match(document.querySelector('.hosts-panel form')!.textContent ?? '', /port 要是/)
  assert.equal(posts(requests).length, 0)
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
  await click(row.querySelector('button[aria-label="刪除主機 dom3ok"]')!)
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
  const proj = (await mock.request('POST', '/projects', { path: '/srv/dom3used', label: 'dom3used-proj', host: 'dom3used' })) as { project_id: string }
  await useStore.getState().refreshState()
  await until(() => [...document.querySelectorAll('.host-row')].some((r) => r.textContent?.includes('dom3used')), '主機列出現')
  const row = [...document.querySelectorAll('.host-row')].find((r) => r.textContent?.includes('dom3used'))!
  await click(row.querySelector('button[aria-label="刪除主機 dom3used"]')!)
  const dialog = document.querySelector('[role=alertdialog]')!
  assert.match(dialog.textContent ?? '', /仍有 1 個 Project/)
  const confirm = buttonByText(dialog, '刪除主機')
  assert.equal(confirm.disabled, true)
  await click(confirm)
  assert.equal(requests.some((r) => r.method === 'DELETE'), false)
  // 共用的 mock 會留給後面的測試檔：把這台主機與它的 Project 收乾淨。
  await mock.request('DELETE', `/projects/${proj.project_id}`)
  await mock.request('DELETE', '/hosts/dom3used')
})

// #1220：每一列的按鈕名稱要帶主機名，讀屏與語音控制才分得出是哪一台。
async function seedTwoHosts() {
  await mock.request('POST', '/hosts', { name: 'm4p', ssh: 'me@m4p' })
  await mock.request('POST', '/hosts', { name: 'box2', ssh: 'me@box2' })
  await useStore.getState().refreshState()
  await until(() => hostNames().includes('box2') && hostNames().includes('m4p'), '兩台主機出現')
}
async function dropTwoHosts() {
  await mock.request('DELETE', '/hosts/m4p')
  await mock.request('DELETE', '/hosts/box2')
}

it('每一台的刪除鈕名稱帶主機名：不會出現好幾顆同名的「刪除主機」（#1220）', async () => {
  await open()
  await seedTwoHosts()
  await until(() => document.querySelector('button[aria-label="刪除主機 m4p"]') !== null, 'm4p 的刪除鈕')
  assert.ok(document.querySelector('button[aria-label="刪除主機 box2"]'))
  assert.equal(document.querySelector('button[aria-label="刪除主機"]'), null, '沒有不帶名字的刪除鈕')
  await dropTwoHosts()
})

it('重連鈕名稱帶主機名（畫面文字不變）；本機那列的開 shell 名稱說明是本機（#1220）', async () => {
  await open()
  await seedTwoHosts()
  await until(() => document.querySelector('button[aria-label="重連 m4p"]') !== null, 'm4p 的重連鈕')
  assert.equal(document.querySelector('button[aria-label="重連 m4p"]')!.textContent?.trim(), '重連')
  assert.ok(document.querySelector('button[aria-label="重連 box2"]'))
  assert.ok(document.querySelector('button[aria-label="在本機開 shell"]'), '本機那列的開 shell')
  await dropTwoHosts()
})

it('結束 shell 的名稱帶 cwd 與 pane：同一台主機開好幾顆 shell 也分得出來（#1220）', async () => {
  mockApi(mock)
  await seedTwoHosts()
  const sh = (await mock.request('POST', '/hosts/m4p/shells', { cwd: '/home/u/proj' })) as { pane_id: string }
  await useStore.getState().refreshState()
  await mount(<HostsPanel />)
  const label = `結束 shell /home/u/proj（pane ${sh.pane_id}）`
  await until(() => document.querySelector(`button[aria-label="${label}"]`) !== null, '結束 shell 鈕帶 cwd 與 pane')
  assert.equal(document.querySelector('button[aria-label="結束這個 shell"]'), null, '沒有不帶名字的結束鈕')
  await api.closeHostShell('m4p', sh.pane_id, true)
  await dropTwoHosts()
})
