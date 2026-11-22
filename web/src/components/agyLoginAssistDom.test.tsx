import test from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, typeInto } from '../testing/domHarness'
import { setupLoginAssistDomTest } from '../testing/loginAssistSetup'
import { useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

const { mock, until, elapse } = setupLoginAssistDomTest()
const assist = () => document.querySelector<HTMLElement>('.login-assist')
const codeInput = () => document.querySelector<HTMLInputElement>('.login-assist-code')!
const submitBtn = () => document.querySelector<HTMLButtonElement>('.login-assist-form button[type="submit"]')!

async function openAgyPane() {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/identities/ag1/login')) as { pane_id: string; cwd: string }
  await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
  await elapse(300)
  return { requests, shell }
}

test('agy 登入 pane：標題寫 agy 身份、「打開登入網站」是 Google 授權網址、有貼 code 的輸入框', async () => {
  await openAgyPane()
  await until(() => assist() !== null, '登入協助條出現')
  assert.match(assist()!.textContent ?? '', /登入 agy 身份 ag1/)
  const link = document.querySelector<HTMLAnchorElement>('a.login-assist-open')!
  assert.match(link.href, /^https:\/\/accounts\.google\.com\/o\/oauth2\/auth\?/)
  assert.equal(link.target, '_blank')
  assert.ok(codeInput())
  assert.equal(document.querySelector('.login-assist-devcode'), null)
})

test('agy 登入 pane：授權碼送到 login/code；錯的碼顯示 agy 的錯誤、輸入框還在', async () => {
  const { requests, shell } = await openAgyPane()
  await until(() => assist() !== null, '登入協助條出現')
  await typeInto(codeInput(), 'bad-code')
  await click(submitBtn())
  await until(() => /Got an error/.test(assist()?.textContent ?? ''), '顯示 agy 的失敗原因')
  assert.ok(document.querySelector('.login-assist-form'))
  await typeInto(codeInput(), '  4/0AXgood-code  ')
  await click(submitBtn())
  await elapse(300)
  const posts = requests.filter((r) => r.method === 'POST' && r.path.endsWith('/login/code'))
  assert.equal(posts.length, 2)
  assert.equal(posts[1].path, `/api/hosts/local/shells/${encodeURIComponent(shell.pane_id)}/login/code`)
  assert.deepEqual(posts[1].body, { code: '4/0AXgood-code' })
})
