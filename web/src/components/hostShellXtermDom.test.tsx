/**
 * 主機 shell 的 xterm 終端（HostShellXterm）：推送 frame 會畫上去；沒有推送（舊 daemon 404）就退回輪詢。
 * 真的掛進 happy-dom，後端是 `MockTransport`；WebSocket 用 `fakeWebSocket()` 假掉，由測試決定何時連上、何時推訊息。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeWebSocket, mockApi, mount, setupDom, teardownDom, unmountAll, until, type FakeRequest, type FakeSocketControl } from '../testing/domHarness'
import { sharedMock } from '../testing/sharedMock'
import { resetStoreForTest } from '../store/store'
import { HostShellXterm } from './HostShellXterm'

const mock = sharedMock
const opened: string[] = []
let ws: FakeSocketControl
let requests: FakeRequest[]

before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})
beforeEach(() => {
  ws = fakeWebSocket()
  requests = mockApi(mock)
})
afterEach(async () => {
  await unmountAll()
  for (const pane of opened.splice(0)) await mock.request('DELETE', `/hosts/local/shells/${encodeURIComponent(pane)}?confirm=true`).catch(() => {})
})

async function openPane(): Promise<string> {
  const shell = (await mock.request('POST', '/hosts/local/shells', {})) as { pane_id: string }
  opened.push(shell.pane_id)
  return shell.pane_id
}

const screen = () => document.querySelector('.xterm-rows')?.textContent ?? ''
const terminalReads = () => requests.filter((r) => r.method === 'GET' && r.path.includes('/terminal') && r.path.includes('format=ansi')).length

test('推送的 frame 畫進終端，並回報 pane 的幾何', async () => {
  const pane = await openPane()
  const geo: [number, number][] = []
  await mount(
    <HostShellXterm
      host="local"
      paneId={pane}
      wrap
      onTyping={() => {}}
      onRead={() => {}}
      onSend={() => {}}
      onGeometry={(c, r) => geo.push([c, r])}
      onStreaming={() => {}} fitScreen={false}
    />,
  )
  await ws.open()
  await ws.message({ type: 'frame', seq: 1, text: 'hello from stream', cols: 80, rows: 24 })
  await until(() => screen().includes('hello from stream'), '推送的畫面要畫上終端')
  assert.deepEqual(geo.at(-1), [80, 24])
})

test('連不上推送（舊 daemon 握手被拒）就每隔一陣子輪詢 terminal?format=ansi', async () => {
  const pane = await openPane()
  await mount(<HostShellXterm host="local" paneId={pane} wrap onTyping={() => {}} onRead={() => {}} onSend={() => {}} onGeometry={() => {}} onStreaming={() => {}} fitScreen={false} />)
  // 不 open：WebSocket 停在 CONNECTING，等同握手沒成功，應該由輪詢補畫面。
  await until(() => terminalReads() >= 2, '沒有推送時要輪詢 terminal?format=ansi')
  assert.ok(requests.some((r) => r.path.includes('format=ansi')))
})
