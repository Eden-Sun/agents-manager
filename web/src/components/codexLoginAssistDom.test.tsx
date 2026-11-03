/**
 * codex 身份的登入協助（裝置碼）：登入 pane 上方是「打開登入網站」＋大字一次性碼（＋複製），沒有貼 code 的輸入框。
 * 真的掛 `HostShellPanel` 進 happy-dom，後端是 `MockTransport`（照 daemon 的樣子：裝置碼流程不接受送 code）。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, setupDom, teardownDom, unmountAll, fakeWebSocket } from '../testing/domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from '../testing/sharedMock'
import { installManualTimers } from '../testing/manualTimers'
import { untilVirtual } from '../testing/untilVirtual'
import { resetStoreForTest, useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

virtualMockTime()
let timers: ReturnType<typeof installManualTimers>
const until = (cond: () => boolean | Promise<boolean>, what: string) => untilVirtual(timers.clock, cond, what, { stepMs: 250, alsoAdvance: advanceMockTime })
const elapse = (ms: number) =>
  act(async () => {
    await timers.clock.advance(ms)
    await advanceMockTime(ms)
  })
beforeEach(() => {
  fakeWebSocket()
  timers = installManualTimers()
})
afterEach(async () => {
  timers.restore()
  await unmountAll()
})
before(setupDom)
const mock = sharedMock
type MockIds = { localIdentityStatus: Record<string, unknown> }
const idsBefore = structuredClone((mock as unknown as MockIds).localIdentityStatus)
after(() => {
  const m = mock as unknown as MockIds
  for (const k of Object.keys(m.localIdentityStatus)) delete m.localIdentityStatus[k]
  Object.assign(m.localIdentityStatus, structuredClone(idsBefore))
  resetStoreForTest()
  teardownDom()
})
const assist = () => document.querySelector<HTMLElement>('.login-assist')

test('codex 登入 pane：一次性碼大字顯示、網址是 codex 裝置頁、沒有貼 code 的輸入框', async () => {
  mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/identities/cx1/login')) as { pane_id: string; cwd: string }
  await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
  await elapse(300)
  await until(() => document.querySelector('.login-assist-devcode') !== null, '一次性碼出現')
  assert.match(assist()!.textContent ?? '', /cx1/)
  assert.equal(document.querySelector('.login-assist-devcode')!.textContent, 'MOCK-7K2Q')
  const link = document.querySelector<HTMLAnchorElement>('a.login-assist-open')!
  assert.equal(link.href, 'https://auth.openai.com/codex/device')
  assert.equal(link.target, '_blank')
  assert.equal(document.querySelector('.login-assist-form'), null, '裝置碼不貼回終端')
  assert.ok(document.querySelector('.login-assist-copy'), '有複製鍵')
})
