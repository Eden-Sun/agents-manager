/**
 * 遠端主機離線要一眼看得到（2026-10-02 使用者「m4p offline，要更明顯一點」）：
 * 頂端紅條（哪台、多久、影響幾顆，點開列 bot、點 bot 會選它）、側欄整組標 host-down 且狀態字是「主機離線」、
 * 聊天區橫幅；主機連回來三處都收掉。
 */
import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { HostOfflineBanner, HostOfflineChatNotice } from './HostOffline'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string, host: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host })
const bot = (id: string, projectId: string) => ({
  id, name: `bot-${id}`, project_id: projectId, kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
})
const m4p = (connected: boolean) => ({
  name: 'm4p', ssh: 'me@m4p', ssh_port: 22, herdr_session: 'agents-manager', remote_path: '', connected,
  error: connected ? null : 'ssh master exited', disconnected_since: connected ? null : new Date(Date.now() - 23 * 60_000).toISOString(),
  attach_command: '', herdr: { server_version: null, protocol: null, protocol_supported: null, cli_version: null, mismatch: false },
  tools: {}, identity_status: {},
})
const running = (id: string) => ({ id: `run-${id}`, bot_id: id, state: 'running', agent_status: 'working' })

beforeEach(() => {
  fakeApi()
  useStore.setState({
    hosts: [m4p(false)],
    projects: [project('p1', 'local'), project('p2', 'm4p')],
    bots: [bot('b1', 'p1'), bot('b2', 'p2'), bot('b3', 'p2')],
    runs: { b2: running('b2'), b3: running('b3') }, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true,
    socket: 'open', selectedBotId: 'b1', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

test('頂端紅條：哪台、離線多久、影響幾顆；點開列出 bot，點下去選那顆', async () => {
  await mount(<HostOfflineBanner />)
  await settle()
  const banner = document.querySelector('.host-offline-banner')!
  assert.ok(banner, '離線就要有紅條')
  assert.match(banner.textContent!, /主機 m4p 離線已 23 分鐘/)
  const toggle = banner.querySelector<HTMLButtonElement>('.hob-toggle')!
  assert.match(toggle.textContent!, /影響 2 顆 bot/)
  await click(toggle)
  const bots = [...document.querySelectorAll<HTMLButtonElement>('.hob-bot')]
  assert.deepEqual(bots.map((b) => b.querySelector('.hob-bot-name')!.textContent), ['bot-b2', 'bot-b3'])
  assert.match(document.querySelector('.hob-note')!.textContent!, /ssh master exited/)
  await click(bots[1])
  assert.equal(useStore.getState().selectedBotId, 'b3')
  await act(async () => useStore.setState({ hosts: [m4p(true)] } as never))
  assert.equal(document.querySelector('.host-offline-banner'), null, '連回來就收掉')
})

test('側欄：離線主機的專案標 host-down、bot 狀態字是「主機離線」不是執行中；本機的不受影響', async () => {
  await mount(<Sidebar />)
  await settle()
  const section = (pid: string) => document.querySelector(`section[data-project-id="${pid}"]`)!
  assert.ok(section('p2').classList.contains('host-down'))
  assert.ok(!section('p1').classList.contains('host-down'))
  assert.match(section('p2').querySelector('.host-badge')!.textContent!, /@m4p 離線/)
  const state = document.querySelector('[data-bot-id="b2"] .bot-state')!
  assert.equal(state.textContent, '主機離線')
  assert.ok(document.querySelector('[data-bot-id="b2"] .lamp-disconnected'), '燈號是斷線灰，不是最後一次的 working')
  await act(async () => useStore.setState({ hosts: [m4p(true)] } as never))
  assert.ok(!section('p2').classList.contains('host-down'))
  assert.notEqual(document.querySelector('[data-bot-id="b2"] .bot-state')?.textContent, '主機離線')
})

test('聊天區橫幅：只有離線主機上的 bot 才有，講清楚送不出去與回來後怎樣', async () => {
  await mount(
    <>
      <HostOfflineChatNotice botId="b1" />
      <HostOfflineChatNotice botId="b2" />
    </>,
  )
  await settle()
  const notices = document.querySelectorAll('.host-offline-chat')
  assert.equal(notices.length, 1, '本機的 b1 不畫')
  assert.match(notices[0].textContent!, /m4p 上，主機離線 23 分鐘：訊息現在送不出去/)
  assert.match(notices[0].textContent!, /連回來後/)
})
