/**
 * WebSocket 重連的退避與 token（假 WebSocket、假計時器）：
 * - daemon 換了 token（重啟、重新產生）之後，ws 連線在握手就被 401 拒絕（瀏覽器只看得到「沒開成」）。以前只有 HTTP 的
 *   GET 撞 401 才會重拿 token，ws 一直拿舊的重試，畫面永遠「斷線」直到使用者重整。
 * - 開著不到幾秒就被關（代理、半壞的 daemon）不能把退避歸零：每次「開成功」都整份重抓 state／對話／額度／草稿。
 * - 一群分頁同時重連（daemon 重啟）的抖動要夠寬，不然全部同一刻打回來。
 */
import { afterEach, test } from 'node:test'
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
  url: string
  constructor(url: string) {
    this.url = url
    FakeWS.instances.push(this)
  }
  get token() {
    return new URL(this.url).searchParams.get('token')
  }
  open() {
    this.readyState = FakeWS.OPEN
    this.onopen?.()
  }
  /** 握手被拒（401）：瀏覽器只會 error → close，沒有 open。 */
  reject() {
    this.readyState = 3
    this.onclose?.()
  }
  drop() {
    this.readyState = 3
    this.onclose?.()
  }
  close() {
    this.readyState = 3
  }
}

const realSetTimeoutG = globalThis.setTimeout
const realClearTimeoutG = globalThis.clearTimeout
const realFetchG = globalThis.fetch
afterEach(() => {
  globalThis.setTimeout = realSetTimeoutG
  globalThis.clearTimeout = realClearTimeoutG
  globalThis.fetch = realFetchG
})

function rig(token: string, sessionToken = 'new') {
  FakeWS.instances = []
  const timers: { fn: () => void; ms: number; live: boolean }[] = []
  const realSetTimeout = globalThis.setTimeout
  const realClearTimeout = globalThis.clearTimeout
  const g = globalThis as unknown as Record<string, unknown>
  g.setTimeout = ((fn: () => void, ms: number) => {
    const t = { fn, ms, live: true }
    timers.push(t)
    return t
  }) as unknown as typeof setTimeout
  g.clearTimeout = ((t: { live: boolean }) => {
    if (t) t.live = false
  }) as unknown as typeof clearTimeout
  g.WebSocket = FakeWS
  g.location = { protocol: 'http:', host: '127.0.0.1:7788' }
  g.window = { addEventListener: () => {}, removeEventListener: () => {} }
  g.document = { visibilityState: 'visible', addEventListener: () => {}, removeEventListener: () => {} }
  const fetched: string[] = []
  g.fetch = (async (input: string) => {
    fetched.push(String(input))
    return {
      ok: true,
      status: 200,
      statusText: 'OK',
      text: async () => JSON.stringify({ token: sessionToken }),
    } as unknown as Response
  }) as unknown as typeof fetch
  let clock = 1_000_000
  const statuses: string[] = []
  const t = new HttpTransport()
  ;(t as unknown as { token: string }).token = token
  const stop = t.openSocket(
    { since: () => 0, onStatus: (s) => statuses.push(s), onFrame: () => {} },
    { now: () => clock, every: () => () => {} },
  )
  return {
    stop: () => {
      stop()
      g.setTimeout = realSetTimeout
      g.clearTimeout = realClearTimeout
    },
    statuses,
    fetched,
    advance: (ms: number) => { clock += ms },
    last: () => FakeWS.instances[FakeWS.instances.length - 1],
    /** 排著的重連 timer 的延遲（最後一個還活著的）。 */
    pendingDelay: () => [...timers].reverse().find((x) => x.live)?.ms ?? null,
    /** 觸發最後一個還活著的 timer。 */
    fireTimer: async () => {
      const t = [...timers].reverse().find((x) => x.live)
      if (!t) throw new Error('沒有排著的重連 timer')
      t.live = false
      t.fn()
      await new Promise((r) => realSetTimeout(r, 0))
    },
  }
}

test('token 換了：ws 握手一直被拒時要自己重拿 token，不能拿舊的重試到天荒地老', async () => {
  const r = rig('old')
  assert.equal(r.last().token, 'old')
  for (let i = 0; i < 4 && r.last().token !== 'new'; i++) {
    r.last().reject()
    await r.fireTimer()
  }
  assert.equal(r.last().token, 'new', `連續握手失敗後下一條連線要帶重拿的 token（拿過：${r.fetched.join(',')}）`)
  assert.ok(r.fetched.includes('/api/session'))
  r.last().open()
  r.stop()
})

test('第一次握手失敗就去打 /api/session 太急（daemon 只是還沒起來）：兩次以上才重拿，而且 single-flight', async () => {
  const r = rig('tok')
  r.last().reject()
  await r.fireTimer()
  assert.equal(r.fetched.length, 0, '才失敗一次，不重拿')
  r.last().reject()
  await r.fireTimer()
  r.last().reject()
  await r.fireTimer()
  assert.ok(r.fetched.filter((u) => u === '/api/session').length >= 1)
  assert.ok(r.fetched.length <= 2, `重拿不能每次重連都打一輪：${r.fetched.length}`)
  r.stop()
})

test('開成功又馬上被關（不到穩定時間）：退避繼續往上，不能每次歸零', async () => {
  const r = rig('tok')
  const delays: number[] = []
  for (let i = 0; i < 5; i++) {
    r.last().open()
    r.advance(1_000) // 只活 1 秒
    r.last().drop()
    delays.push(r.pendingDelay() ?? -1)
    await r.fireTimer()
  }
  assert.ok(delays[4] >= delays[0] * 4, `退避要長大：${delays.join(',')}`)
  r.stop()
})

test('開著夠久（穩定）才歸零：長時間連線後的下一次斷線從頭退避', async () => {
  const r = rig('tok')
  for (let i = 0; i < 4; i++) {
    r.last().reject()
    await r.fireTimer()
  }
  r.last().open()
  r.advance(60_000)
  r.last().drop()
  assert.ok((r.pendingDelay() ?? 9999) <= 400, `穩定連線後斷線，第一次重連該很快：${r.pendingDelay()}`)
  r.stop()
})

test('一群分頁同時重連：同一輪退避的延遲要攤得夠開（不是全部落在同一個 150ms 內）', async () => {
  const realRandom = Math.random
  try {
    const at = async (rand: number) => {
      Math.random = () => rand
      const r = rig('tok')
      for (let i = 0; i < 6; i++) {
        r.last().reject()
        if (i < 5) await r.fireTimer()
      }
      const d = r.pendingDelay() ?? 0
      r.stop()
      return d
    }
    const lo = await at(0)
    const hi = await at(0.999)
    assert.ok(hi - lo >= 1000, `封頂那一輪的延遲範圍只有 ${lo}–${hi}`)
    assert.ok(hi <= 3_200, `封頂 3 秒左右（${hi}）：daemon 回來得快，畫面不能卡太久`)
  } finally {
    Math.random = realRandom
  }
})
