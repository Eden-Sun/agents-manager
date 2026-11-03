/**
 * codex 登入會先刪掉那個帳號原本的 auth.json（2026-11-03 實測）：已登入就先問，取消就一個請求都不發；未登入、claude 直接走。
 * 後端是 mock（不碰真的 CODEX_HOME）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { startCliLogin } from './cliLogin'
import { codexLoginReplacesLogin } from './codexLoginGuard'

virtualMockTime()
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
const tick = () => new Promise((r) => setTimeout(r, 20))

const codexStatus = (name: string, loggedIn: boolean | null) => ({
  name,
  kind: 'codex',
  logged_in: loggedIn,
  reason: null,
  account: null,
  plan: null,
  source: 'config',
  config_dir: null,
  login_needed: null,
})

function seed({ defaultCodex, identities }: { defaultCodex: boolean | null; identities: Record<string, boolean | null> }) {
  const s = useStore.getState()
  useStore.setState({
    localTools: { ...s.localTools, codex: { installed: true, path: null, version: null, logged_in: defaultCodex } },
    localIdentityStatus: Object.fromEntries(Object.entries(identities).map(([n, li]) => [n, codexStatus(n, li)])) as never,
    projects: [{ id: 'p-cli', host: 'local' }] as never,
    bots: [{ id: 'b-cx', project_id: 'p-cli', kind: 'codex', identity: null }] as never,
  })
}

test('純判斷：只有 codex 且已登入才要問', () => {
  assert.equal(codexLoginReplacesLogin('codex', true), true)
  assert.equal(codexLoginReplacesLogin('codex', false), false, '未登入沒有東西可以失效')
  assert.equal(codexLoginReplacesLogin('codex', null), false, '問不到不擋')
  assert.equal(codexLoginReplacesLogin('claude', true), false, 'claude 不受影響')
})

it('預設帳號已登入的 codex：先問；取消就不開 shell、不送任何指令', async () => {
  const requests = mockApi(sharedMock)
  await closeLocalShells()
  seed({ defaultCodex: true, identities: {} })
  const pending = startCliLogin('b-cx')
  await tick()
  assert.ok(useStore.getState().codexLoginAsk, '跳確認框')
  assert.equal(requests.filter((r) => r.method === 'POST' && r.path.endsWith('/shells')).length, 0, '還沒開 shell')
  useStore.getState().answerCodexLoginAsk(false)
  assert.equal(await pending, false)
  assert.equal(requests.filter((r) => r.method === 'POST' && r.path.endsWith('/shells')).length, 0, '取消：什麼都沒開')
  assert.equal(useStore.getState().codexLoginAsk, null)
})

it('預設帳號已登入的 codex：確定才開 shell 並打登入指令', async () => {
  const requests = mockApi(sharedMock)
  await closeLocalShells()
  seed({ defaultCodex: true, identities: {} })
  const pending = startCliLogin('b-cx')
  await tick()
  useStore.getState().answerCodexLoginAsk(true)
  assert.equal(await pending, true)
  const opened = requests.filter((r) => r.method === 'POST' && r.path.endsWith('/shells'))
  assert.equal(opened.length, 1)
  const typed = requests.find((r) => r.method === 'POST' && r.path.endsWith('/text'))
  assert.match(String((typed?.body as { text?: string } | undefined)?.text ?? ''), /codex login --device-auth/)
})

it('命名身份已登入的 codex：先問；取消不發登入請求，確定才發', async () => {
  const requests = mockApi(sharedMock)
  await closeLocalShells()
  seed({ defaultCodex: null, identities: { cx1: true } })
  const cancelled = useStore.getState().loginIdentity('local', 'cx1')
  await tick()
  assert.ok(useStore.getState().codexLoginAsk, '跳確認框')
  assert.equal(requests.filter((r) => r.path.includes('/identities/cx1/login')).length, 0)
  useStore.getState().answerCodexLoginAsk(false)
  assert.equal(await cancelled, false)
  assert.equal(requests.filter((r) => r.path.includes('/identities/cx1/login')).length, 0, '取消：沒有登入請求')

  const started = useStore.getState().loginIdentity('local', 'cx1')
  await tick()
  useStore.getState().answerCodexLoginAsk(true)
  await started
  assert.equal(requests.filter((r) => r.path.includes('/identities/cx1/login')).length, 1, '確定：發出登入請求')
})

it('未登入的 codex 與已登入的 claude：不問，直接開始', async () => {
  const requests = mockApi(sharedMock)
  await closeLocalShells()
  seed({ defaultCodex: false, identities: { cx9: null } })
  await startCliLogin('b-cx')
  assert.equal(useStore.getState().codexLoginAsk, null, '未登入：不問')
  assert.ok(requests.some((r) => r.method === 'POST' && r.path.endsWith('/shells')), '直接開了 shell')

  useStore.setState({ localIdentityStatus: { cc1: { ...codexStatus('cc1', true), kind: 'claude' } } as never })
  void useStore.getState().loginIdentity('local', 'cc1')
  await tick()
  assert.equal(useStore.getState().codexLoginAsk, null, 'claude 不問')
  assert.ok(requests.some((r) => r.path.includes('/identities/cc1/login')))
})

it('還有一個沒答的確認又來了：舊的算取消，不讓它永遠懸著', async () => {
  mockApi(sharedMock)
  await closeLocalShells()
  seed({ defaultCodex: null, identities: { cx1: true } })
  const first = useStore.getState().loginIdentity('local', 'cx1')
  await tick()
  const second = useStore.getState().loginIdentity('local', 'cx1')
  await tick()
  assert.equal(await first, false, '舊的那個被取消')
  useStore.getState().answerCodexLoginAsk(false)
  assert.equal(await second, false)
})
