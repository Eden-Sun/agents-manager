/**
 * issue #760：WS 沒有存活偵測，半開連線（睡眠、NAT／tailscale 逾時）下畫面凍住卻顯示已連線。
 * 這裡用假 WebSocket 與假時鐘驅動 `openSocket`：靜默太久、或切回前景時靜默超過心跳間隔，都要主動丟掉舊 socket 重連。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { HttpTransport } from './transport'

class FakeWS {
  static OPEN = 1
  static CONNECTING = 0
  static instances: FakeWS[] = []
  readyState = 0
  onopen: (() => void) | null = null
  onmessage: ((ev: { data: string }) => void) | null = null
  onerror: (() => void) | null = null
  onclose: (() => void) | null = null
  closed = false
  url: string
  constructor(url: string) {
    this.url = url
    FakeWS.instances.push(this)
  }
  open() {
    this.readyState = FakeWS.OPEN
    this.onopen?.()
  }
  push(frame: unknown) {
    this.onmessage?.({ data: JSON.stringify(frame) })
  }
  close() {
    this.closed = true
    this.readyState = 3
  }
}

function rig() {
  FakeWS.instances = []
  const listeners: Record<string, (() => void)[]> = {}
  const g = globalThis as unknown as Record<string, unknown>
  g.WebSocket = FakeWS
  g.location = { protocol: 'http:', host: '127.0.0.1:7788' }
  g.window = {
    addEventListener: (t: string, fn: () => void) => (listeners[t] ??= []).push(fn),
    removeEventListener: () => {},
  }
  g.document = {
    visibilityState: 'visible',
    addEventListener: (t: string, fn: () => void) => (listeners[`doc:${t}`] ??= []).push(fn),
    removeEventListener: () => {},
  }
  let clock = 1_000_000
  let tick: (() => void) | null = null
  const statuses: string[] = []
  const frames: string[] = []
  const t = new HttpTransport()
  ;(t as unknown as { token: string }).token = 'tok'
  const stop = t.openSocket(
    { since: () => 0, onStatus: (s) => statuses.push(s), onFrame: (f) => frames.push(f.type) },
    { now: () => clock, every: (fn) => { tick = fn; return () => { tick = null } } },
  )
  return {
    stop,
    statuses,
    frames,
    advance: (ms: number) => { clock += ms },
    watchdog: () => tick?.(),
    fire: (type: string) => listeners[type]?.forEach((fn) => fn()),
    fireDoc: (type: string) => listeners[`doc:${type}`]?.forEach((fn) => fn()),
    last: () => FakeWS.instances[FakeWS.instances.length - 1],
  }
}

test('心跳 ping 幀算存活證據但不丟給 store；一直有幀就不重連', () => {
  const r = rig()
  r.last().open()
  for (let i = 0; i < 6; i++) {
    r.advance(20_000)
    r.last().push({ type: 'ping' })
    r.watchdog()
  }
  assert.equal(FakeWS.instances.length, 1, '120 秒內每 20 秒一個 ping：連線健康，不能重連')
  assert.deepEqual(r.frames, [], 'ping 不是事件，不進 store 的 handleFrame')
  r.last().push({ type: 'bot_changed', seq: 3 })
  assert.deepEqual(r.frames, ['bot_changed'])
  r.stop()
})

test('開著的 socket 靜默超過 60 秒：判定半開，關掉舊的、走既有重連（since 由 open handler 補洞）', () => {
  const r = rig()
  r.last().open()
  r.last().push({ type: 'ping' }) // 新 daemon：連上後 20 秒內就會有第一個 ping（舊 daemon 見下面那一則）
  const first = r.last()
  r.advance(30_000)
  r.watchdog()
  assert.equal(FakeWS.instances.length, 1, '30 秒還在容許內')
  r.advance(31_000)
  r.watchdog()
  assert.equal(first.closed, true, '舊 socket 要關掉')
  assert.equal(FakeWS.instances.length, 2, '馬上開新的')
  assert.equal(r.statuses.at(-1), 'connecting')
  // 舊 socket 的 onclose 晚到不能再排第二條連線。
  first.onclose?.()
  assert.equal(FakeWS.instances.length, 2)
  r.stop()
})

test('切回前景：readyState 雖是 OPEN，但靜默超過心跳間隔就重連；剛有幀就不動', () => {
  const r = rig()
  r.last().open()
  r.last().push({ type: 'ping' })
  r.advance(10_000)
  r.fireDoc('visibilitychange')
  r.fire('focus')
  r.fire('online')
  assert.equal(FakeWS.instances.length, 1, '10 秒內有活動，不必重連')
  r.advance(30_000) // 共靜默 40 秒，>35 秒（心跳 20 秒＋餘裕）
  r.fireDoc('visibilitychange')
  assert.equal(FakeWS.instances.length, 2, '切回前景要重連，不能只看 readyState')
  r.stop()
})

test('分頁在背景時不用開心跳檢查去重連（回前景那一刻才判斷）', () => {
  const r = rig()
  r.last().open()
  r.last().push({ type: 'ping' })
  ;(globalThis as unknown as { document: { visibilityState: string } }).document.visibilityState = 'hidden'
  r.advance(120_000)
  r.watchdog()
  assert.equal(FakeWS.instances.length, 1, '背景分頁的 timer 被節流、frame 也可能被凍住，靜默不代表斷線')
  ;(globalThis as unknown as { document: { visibilityState: string } }).document.visibilityState = 'visible'
  r.fireDoc('visibilitychange')
  assert.equal(FakeWS.instances.length, 2)
  r.stop()
})

test('舊 daemon 不送 ping：安靜的前景分頁不能每分鐘重連（每次重連都整份重抓 state／對話）', () => {
  const r = rig()
  r.last().open()
  for (let i = 0; i < 10; i++) {
    r.advance(30_000)
    r.watchdog()
  }
  assert.equal(FakeWS.instances.length, 1, '5 分鐘沒有任何幀：舊 daemon 本來就不送心跳，靜默不是斷線的證據')
  r.advance(60_000)
  r.fireDoc('visibilitychange')
  r.fire('focus')
  assert.equal(FakeWS.instances.length, 1, '切回前景也一樣：沒見過 ping 就不拿靜默當證據')
  r.stop()
})

test('見過 ping 之後才信靜默：重連後的新連線（還沒收到第一個 ping）照樣有死線偵測', () => {
  const r = rig()
  r.last().open()
  r.last().push({ type: 'ping' })
  r.advance(61_000)
  r.watchdog()
  assert.equal(FakeWS.instances.length, 2, '見過 ping 的 daemon 靜默 61 秒＝半開')
  r.last().open() // 新連線，還沒收到它的第一個 ping
  r.advance(61_000)
  r.watchdog()
  assert.equal(FakeWS.instances.length, 3, '這個 daemon 會送 ping（同一個分頁已經見過），新連線也不能靜默 60 秒')
  r.stop()
})
