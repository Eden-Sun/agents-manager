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
/** 跟 `ChatPanel` 一樣：面板只在 `shellView` 指到它時才畫；pane 被判定不見、`shellView` 清掉，面板就跟著拆掉。 */
function ShellViewHost({ host, paneId, cwd }: { host: string; paneId: string; cwd: string }) {
  const view = useStore((s) => s.shellView)
  if (!view || view.host !== host || view.paneId !== paneId) return null
  return <HostShellPanel host={host} paneId={paneId} cwd={cwd} embedded />
}

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

test('CLI 結束、pane 收掉：協助條切到結果（已登入），面板不被拆掉、不跳「pane 已經關掉了」', async () => {
  mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/identities/cx1/login')) as { pane_id: string; cwd: string }
  useStore.getState().viewHostShell({ host: 'local', pane_id: shell.pane_id, cwd: shell.cwd } as never)
  await mount(<ShellViewHost host="local" paneId={shell.pane_id} cwd={shell.cwd} />)
  await elapse(300)
  await until(() => document.querySelector('.login-assist-devcode') !== null, '一次性碼出現')
  // 使用者到網站授權後，CLI 結束、daemon 收掉登入 pane。
  await mock.request('DELETE', `/hosts/local/shells/${encodeURIComponent(shell.pane_id)}`)
  await until(() => document.querySelector('.login-assist-done') !== null, '協助條切到結果狀態')
  assert.match(document.querySelector('.login-assist-done')!.textContent ?? '', /已登入|登入程序已結束/)
  assert.equal(document.querySelector('.login-assist-devcode'), null, '結果狀態不再顯示舊的碼')
  await elapse(3000)
  assert.ok(document.querySelector('.login-assist-done'), '過一陣子結果還在')
  assert.ok(!useStore.getState().notices.some((n) => n.text.includes('已經關掉')), '登入 pane 收掉不當成「pane 不見」')
  assert.ok(document.querySelector('.shell-pane'), '面板還開著，使用者看得到結果')
  assert.ok(useStore.getState().shellView, 'shellView 沒有被清掉')
})
