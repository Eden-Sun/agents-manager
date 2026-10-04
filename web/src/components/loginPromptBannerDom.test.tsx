/**
 * 身分被登出的主動提示條（#838 補充）：真的掛 `LoginPromptBanner` 進 happy-dom，store 直接種資料。
 * 「立即登入」走 `loginIdentity`（打開登入協助面板）；「先關掉」只對這一次登出有效。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Host, IdentityStatus, Project } from '../api/types'
import { LoginPromptBanner } from './LoginPromptBanner'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const st = (over: Partial<IdentityStatus> = {}): IdentityStatus => ({
  name: 'cc1', kind: 'claude', logged_in: true, reason: null, account: 'fake@example.test', plan: null, source: 'config', config_dir: null, login_needed: null, ...over,
})

const calls: [string, string][] = []

function seed(status: IdentityStatus) {
  resetStoreForTest()
  calls.length = 0
  useStore.setState({
    projects: [{ id: 'p1', label: 'p', path: '/p', host: 'm4p' } as Project],
    bots: [{ id: 'b1', name: 'ops', project_id: 'p1', kind: 'claude', identity: 'cc1' } as Bot],
    hosts: [{ name: 'm4p', connected: true, identity_status: { cc1: status } } as unknown as Host],
    connected: true,
    loginIdentity: async (host: string, identity: string) => {
      calls.push([host, identity])
      return true
    },
  })
}

const banner = () => document.querySelector<HTMLElement>('.login-prompt-banner')
const setStatus = (status: IdentityStatus) =>
  act(async () => {
    useStore.setState({ hosts: [{ name: 'm4p', connected: true, identity_status: { cc1: status } } as unknown as Host] })
  })

test('已登入：沒有提示條', async () => {
  seed(st())
  await mount(<LoginPromptBanner />)
  assert.equal(banner(), null)
})

test('遠端探測問不出未登入、但回合授權失敗：出現「cc1（m4p）已登出」，點「立即登入」打開那個身分的登入', async () => {
  seed(st({ logged_in: null, login_needed: { since: 'T1', via: 'turn_auth_failure' } }))
  await mount(<LoginPromptBanner />)
  assert.ok(banner(), '出現提示條')
  assert.match(banner()!.textContent ?? '', /cc1（m4p）已登出/)
  assert.match(banner()!.textContent ?? '', /1 顆 bot/)
  await click(document.querySelector<HTMLButtonElement>('.lpb-login')!)
  assert.deepEqual(calls, [['m4p', 'cc1']])
})

test('先關掉：同一次登出不再出現；登入成功後提示消失，下一次被登出又會提示', async () => {
  seed(st({ logged_in: null, login_needed: { since: 'T1', via: 'turn_auth_failure' } }))
  await mount(<LoginPromptBanner />)
  await click(document.querySelector<HTMLButtonElement>('.lpb-dismiss')!)
  assert.equal(banner(), null, '關掉了')
  // daemon 又推一次同一筆快照（例如別的事件觸發 host_changed）：還是關著，不洗版。
  await setStatus(st({ logged_in: null, login_needed: { since: 'T1', via: 'turn_auth_failure' } }))
  assert.equal(banner(), null)
  // 登入成功：記號沒了。
  await setStatus(st())
  assert.equal(banner(), null)
  assert.deepEqual(useStore.getState().loginPromptDismissed, {}, '關掉的記錄跟著清掉')
  // 之後探測說又被登出：再提示。
  await setStatus(st({ logged_in: false }))
  assert.ok(banner(), '新的一次登出：再提示')
  // 同一個身分的另一次回合授權失敗（新的 since）也會再提示。
  await click(document.querySelector<HTMLButtonElement>('.lpb-dismiss')!)
  assert.equal(banner(), null)
  await setStatus(st({ logged_in: false, login_needed: { since: 'T2', via: 'turn_auth_failure' } }))
  assert.ok(banner())
})

test('沒有 bot 綁著這個身分、或主機連不上：不提示', async () => {
  seed(st({ logged_in: false }))
  useStore.setState({ bots: [] })
  await mount(<LoginPromptBanner />)
  assert.equal(banner(), null)
  await act(async () => {
    useStore.setState({
      bots: [{ id: 'b1', name: 'ops', project_id: 'p1', kind: 'claude', identity: 'cc1' } as Bot],
      hosts: [{ name: 'm4p', connected: false, identity_status: { cc1: st({ logged_in: false }) } } as unknown as Host],
    })
  })
  assert.equal(banner(), null)
})
