/**
 * 終端分頁的自動刷新節奏：bot 工作中每 1 秒、閒置 3 秒；背景分頁不抓；上一趟沒回來就跳過，不疊請求。
 * interval 用假的 `window.setInterval` 記下秒數、由測試手動觸發，不靠真實時間。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { TerminalSnapshot } from '../api/types'
import { TerminalTab } from './TerminalTab'

interface FakeTimer {
  id: number
  ms: number
  fn: () => void
  cleared: boolean
}

// 這些在 `before(setupDom)` 之後才抓（模組載入時 window 還不存在）。
let realSetInterval: typeof window.setInterval
let realClearInterval: typeof window.clearInterval
let hiddenDescriptor: PropertyDescriptor | undefined
let timers: FakeTimer[] = []
let hidden = false

function installFakeInterval() {
  timers = []
  let next = 1000
  window.setInterval = ((fn: () => void, ms?: number) => {
    const id = ++next
    timers.push({ id, ms: ms ?? 0, fn, cleared: false })
    return id
  }) as unknown as typeof window.setInterval
  window.clearInterval = ((id: number) => {
    const t = timers.find((x) => x.id === id)
    if (t) t.cleared = true
  }) as unknown as typeof window.clearInterval
}

/** 目前有效的那顆 interval（沒被 clear 的最後一顆）。 */
const live = () => timers.filter((t) => !t.cleared)
const activeMs = () => {
  const l = live()
  assert.equal(l.length, 1, `應該只有一顆有效的 interval，實際 ${l.length} 顆`)
  return l[0].ms
}
const tick = () => {
  const l = live()
  assert.equal(l.length, 1)
  l[0].fn()
}

interface Pending {
  botId: string
  resolve: (s: TerminalSnapshot) => void
}
function fakeReadTerminal() {
  const calls: Pending[] = []
  const read = (botId: string) =>
    new Promise<TerminalSnapshot>((resolve) => calls.push({ botId, resolve }))
  return { calls, read }
}
const snap = (text: string) => ({ text, pane_id: 'pane-A', columns: 120, rows: 30 }) as unknown as TerminalSnapshot

before(async () => {
  await setupDom()
  realSetInterval = window.setInterval
  realClearInterval = window.clearInterval
  hiddenDescriptor = Object.getOwnPropertyDescriptor(document, 'hidden')
})
afterEach(async () => {
  await unmountAll()
  window.setInterval = realSetInterval
  window.clearInterval = realClearInterval
  if (hiddenDescriptor) Object.defineProperty(document, 'hidden', hiddenDescriptor)
  hidden = false
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

function setHidden(v: boolean) {
  hidden = v
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden })
}

/** 掛上元件並讓初次抓取回來，好讓之後的自動刷新不會被「在途」擋住。 */
async function mountReady(botId = 'A') {
  const { calls, read } = fakeReadTerminal()
  await act(() => useStore.setState({ readTerminal: read } as never))
  await mount(<TerminalTab botId={botId} />)
  await until(() => calls.length >= 1, '初次抓取發出')
  await act(async () => calls[0].resolve(snap('FIRST')))
  await until(() => document.querySelector('pre.term')?.textContent?.includes('FIRST') ?? false, '初次畫面出現')
  return { calls, read }
}

test('bot 工作中：自動刷新每 1 秒；閒置：每 3 秒', async () => {
  installFakeInterval()
  await act(() => useStore.setState({ runs: { A: { agent_status: 'idle' } } } as never))
  await mountReady()
  assert.equal(activeMs(), 3000, '閒置是 3 秒')

  await act(async () => useStore.setState({ runs: { A: { agent_status: 'working' } } } as never))
  assert.equal(activeMs(), 1000, '開始工作後改成 1 秒')

  await act(async () => useStore.setState({ runs: { A: { agent_status: 'idle' } } } as never))
  assert.equal(activeMs(), 3000, '工作結束後回到 3 秒')
})

test('換節奏只重排 interval，不會因此多抓一次（不閃「刷新中…」）', async () => {
  installFakeInterval()
  await act(() => useStore.setState({ runs: { A: { agent_status: 'idle' } } } as never))
  const { calls } = await mountReady()
  const before = calls.length
  await act(async () => useStore.setState({ runs: { A: { agent_status: 'working' } } } as never))
  await act(async () => {})
  assert.equal(calls.length, before, '切到工作中不發額外請求')
})

test('工作中：每一下 tick 都抓一次（回來後才抓下一次）', async () => {
  installFakeInterval()
  await act(() => useStore.setState({ runs: { A: { agent_status: 'working' } } } as never))
  const { calls } = await mountReady()
  setHidden(false)
  const before = calls.length
  await act(async () => tick())
  assert.equal(calls.length, before + 1, 'tick 發出一次請求')
  await act(async () => calls[calls.length - 1].resolve(snap('TICK-1')))
  await act(async () => tick())
  assert.equal(calls.length, before + 2, '上一趟回來後，下一次 tick 再抓')
})

test('上一趟還沒回來：tick 跳過，不疊請求', async () => {
  installFakeInterval()
  await act(() => useStore.setState({ runs: { A: { agent_status: 'working' } } } as never))
  const { calls } = await mountReady()
  setHidden(false)
  const before = calls.length
  await act(async () => tick())
  await act(async () => tick())
  await act(async () => tick())
  assert.equal(calls.length, before + 1, '在途中的 tick 全部跳過')
})

test('背景分頁（document.hidden）：工作中也不抓', async () => {
  installFakeInterval()
  await act(() => useStore.setState({ runs: { A: { agent_status: 'working' } } } as never))
  const { calls } = await mountReady()
  setHidden(true)
  const before = calls.length
  await act(async () => tick())
  await act(async () => tick())
  assert.equal(calls.length, before, '隱藏時 tick 不發請求')
})
