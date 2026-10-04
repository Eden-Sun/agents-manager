/**
 * 手機版重新登入 claude 身分（#838）：登入 pane 上方的「打開登入網站」與 code 輸入框（真的掛 `HostShellPanel` 進 happy-dom，
 * 後端是 `MockTransport`，它照 daemon 的樣子只認登入 pane、畫面不在等 code 就 409）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const mock = sharedMock
const assist = () => document.querySelector<HTMLElement>('.login-assist')
const openLink = () => document.querySelector<HTMLAnchorElement>('a.login-assist-open')
const codeInput = () => document.querySelector<HTMLInputElement>('.login-assist-code')!
const submitBtn = () => document.querySelector<HTMLButtonElement>('.login-assist-form button[type="submit"]')!

async function openLoginPane() {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/identities/cc1/login')) as { pane_id: string; cwd: string }
  await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
  await settle(300)
  return { requests, shell }
}

test('登入 pane：給「打開登入網站」（新分頁、daemon 給的網址）與 code 輸入框；畫面在等 code 就能送', async () => {
  const { requests, shell } = await openLoginPane()
  await until(() => assist() !== null, '登入協助條出現')
  assert.match(assist()!.textContent ?? '', /cc1/)
  const link = openLink()!
  assert.match(link.href, /^https:\/\/claude\.com\/cai\/oauth\/authorize\?/)
  assert.equal(link.target, '_blank')
  assert.match(link.rel, /noopener/)
  assert.equal(submitBtn().disabled, true, '沒貼 code 不能送')

  await typeInto(codeInput(), 'a b')
  assert.equal(submitBtn().disabled, true, '格式不對不能送')
  await typeInto(codeInput(), '  fake-code#state1  ')
  assert.equal(submitBtn().disabled, false)
  await click(submitBtn())
  await settle(300)
  const posts = requests.filter((r) => r.method === 'POST' && r.path.endsWith('/login/code'))
  assert.equal(posts.length, 1)
  assert.equal(posts[0].path, `/api/hosts/local/shells/${encodeURIComponent(shell.pane_id)}/login/code`)
  assert.deepEqual(posts[0].body, { code: 'fake-code#state1' }, '去頭尾空白、只送 code')
  await until(() => !document.querySelector('.login-assist-form'), '送出後 CLI 結束：換成登入狀態')
  assert.match(assist()!.textContent ?? '', /已登入|登入程序已結束/)
})

test('CLI 回報登入失敗：把 Login failed 那句顯示出來，輸入框還在、可以再貼', async () => {
  await openLoginPane()
  await until(() => assist() !== null, '登入協助條出現')
  await typeInto(codeInput(), 'bad-code')
  await click(submitBtn())
  await until(() => /Login failed/.test(assist()?.textContent ?? ''), '顯示失敗原因')
  assert.ok(document.querySelector('.login-assist-form'), '還能再貼一次')
})

test('一般 shell（不是 daemon 開的登入 pane）：不畫協助條，也不再重複問', async () => {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/shells', {})) as { pane_id: string; cwd: string }
  await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
  await settle(300)
  assert.equal(assist(), null)
  const asked = requests.filter((r) => r.method === 'GET' && r.path.endsWith('/login')).length
  await settle(3000)
  assert.equal(requests.filter((r) => r.method === 'GET' && r.path.endsWith('/login')).length, asked, '404 之後不再輪詢')
})
