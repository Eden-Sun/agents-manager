/**
 * 主機 shell 的 xterm 終端（HostShellXterm）：推送 frame 會畫上去；沒有推送（舊 daemon 404）就退回輪詢。
 * 真的掛進 happy-dom，後端是 `MockTransport`；WebSocket 用 `fakeWebSocket()` 假掉，由測試決定何時連上、何時推訊息。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeWebSocket, mockApi, mount, setupDom, teardownDom, unmountAll, until, type FakeRequest, type FakeSocketControl } from '../testing/domHarness'
import { sharedMock } from '../testing/sharedMock'
import { resetStoreForTest } from '../store/store'
import { FitAddon } from '@xterm/addon-fit'
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

const b64 = (s: string) => Buffer.from(s, 'utf8').toString('base64')
const sizeMsgs = () => ws.sent().filter((m) => (m as { type?: unknown }).type === 'size') as { cols: number; rows: number }[]

test('串流 raw：full 幀先清再畫；增量幀接在後面', async () => {
  const pane = await openPane()
  const modes: boolean[] = []
  await mount(
    <HostShellXterm host="local" paneId={pane} wrap onTyping={() => {}} onRead={() => {}} onSend={() => {}} onGeometry={() => {}} onStreaming={() => {}} onObserve={(on) => modes.push(on)} fitScreen={false} />,
  )
  await ws.open()
  await ws.message({ type: 'raw', seq: 1, full: true, cols: 80, rows: 24, data: b64('old screen') })
  await until(() => screen().includes('old screen'), 'full 幀要畫上終端')
  await ws.message({ type: 'raw', seq: 2, full: true, cols: 80, rows: 24, data: b64('new screen') })
  await until(() => screen().includes('new screen') && !screen().includes('old screen'), 'full 幀要先 reset 再寫，舊畫面不留 SCREEN=' + JSON.stringify(screen()))
  await ws.message({ type: 'raw', seq: 3, full: false, cols: 80, rows: 24, data: b64(' +delta') })
  await until(() => screen().includes('new screen +delta'), '增量幀直接接在後面 SCREEN=' + JSON.stringify(screen()))
  assert.deepEqual(modes, [true])
})

test('串流 raw 的 UTF-8 位元組交給 xterm 解碼（中文不會變亂碼）', async () => {
  const pane = await openPane()
  await mount(<HostShellXterm host="local" paneId={pane} wrap onTyping={() => {}} onRead={() => {}} onSend={() => {}} onGeometry={() => {}} onStreaming={() => {}} fitScreen={false} />)
  await ws.open()
  await ws.message({ type: 'raw', seq: 1, full: true, cols: 80, rows: 24, data: b64('主機 shell') })
  await until(() => screen().includes('主機 shell'), 'UTF-8 中文要正確顯示')
})

test('observe 模式連上就送 size（面板大小），之後收到 frame 切回舊模式', async () => {
  // happy-dom 量不出字寬，fit 算不出 cols×rows；這裡假設面板是 120×40。
  const realPropose = FitAddon.prototype.proposeDimensions
  FitAddon.prototype.proposeDimensions = () => ({ cols: 120, rows: 40 })
  try {
    const pane = await openPane()
    const modes: boolean[] = []
    await mount(
      <HostShellXterm host="local" paneId={pane} wrap onTyping={() => {}} onRead={() => {}} onSend={() => {}} onGeometry={() => {}} onStreaming={() => {}} onObserve={(on) => modes.push(on)} fitScreen={false} />,
    )
    await ws.open()
    assert.deepEqual(sizeMsgs(), [{ type: 'size', cols: 120, rows: 40 }], '連上就送一次面板大小')
    await ws.message({ type: 'raw', seq: 1, full: true, cols: 80, rows: 24, data: b64('observe') })
    await until(() => screen().includes('observe'), 'raw 要畫上')
    await ws.message({ type: 'frame', seq: 2, text: 'legacy frame', cols: 80, rows: 24 })
    await until(() => screen().includes('legacy frame'), '舊 frame 仍要畫上（退回舊模式）')
    assert.deepEqual(modes, [true, false])
  } finally {
    FitAddon.prototype.proposeDimensions = realPropose
  }
})

test('frame 模式（舊 daemon）不送 size 以外的東西，行為照舊', async () => {
  const pane = await openPane()
  await mount(<HostShellXterm host="local" paneId={pane} wrap onTyping={() => {}} onRead={() => {}} onSend={() => {}} onGeometry={() => {}} onStreaming={() => {}} fitScreen={false} />)
  await ws.open()
  await ws.message({ type: 'frame', seq: 1, text: 'plain frame', cols: 80, rows: 24 })
  await until(() => screen().includes('plain frame'), 'frame 要畫上')
  assert.ok(ws.sent().every((m) => (m as { type?: unknown }).type === 'size'))
})

test('observe 模式送字不帶 echo=ansi（畫面由串流更新）', async () => {
  const pane = await openPane()
  await mount(<HostShellXterm host="local" paneId={pane} wrap onTyping={() => {}} onRead={() => {}} onSend={() => {}} onGeometry={() => {}} onStreaming={() => {}} fitScreen={false} />)
  await ws.open()
  await ws.message({ type: 'raw', seq: 1, full: true, cols: 80, rows: 24, data: b64('$ ') })
  await until(() => screen().includes('$'), 'raw 要畫上')
  const ta = document.querySelector('.xterm-helper-textarea') as HTMLTextAreaElement | null
  assert.ok(ta, '終端的輸入框要在')
  ta.focus()
  ta.value = 'x'
  ta.dispatchEvent(new InputEvent('input', { data: 'x', inputType: 'insertText', bubbles: true }))
  await until(() => requests.some((r) => r.method === 'POST' && r.path.includes('/text')), '送字要打 text 端點')
  const sent = requests.filter((r) => r.method === 'POST' && r.path.includes('/text'))
  assert.ok(sent.every((r) => !r.path.includes('echo=')), 'observe 模式不要 echo=ansi')
})

test('observe 退回快照：第一張 frame 跟 observe 前的畫面相同也要整屏重畫（#2075）', async () => {
  const pane = await openPane()
  await mount(<HostShellXterm host="local" paneId={pane} wrap onTyping={() => {}} onRead={() => {}} onSend={() => {}} onGeometry={() => {}} onStreaming={() => {}} fitScreen={false} />)
  await ws.open()
  await ws.message({ type: 'frame', seq: 1, text: 'prompt A', cols: 80, rows: 24 })
  await until(() => screen().includes('prompt A'), '第一張 frame 要畫上')
  await ws.message({ type: 'raw', seq: 2, full: true, cols: 80, rows: 24, data: b64('stale observe output') })
  await until(() => screen().includes('stale observe output') && !screen().includes('prompt A'), 'raw 要蓋掉畫面')
  await ws.message({ type: 'frame', seq: 3, text: 'prompt A', cols: 80, rows: 24 })
  await until(() => screen().includes('prompt A') && !screen().includes('stale observe output'), '退回快照後不能留下 observe 的舊列')
})
