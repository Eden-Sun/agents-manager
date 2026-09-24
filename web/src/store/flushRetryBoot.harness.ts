/**
 * `flushRetry.test.ts` 的整合跑道（issue #530）：真的 import `store.ts`、跑 `bootstrap()`、開假 socket
 * 推真的 frame，daemon 由假 fetch 扮演，`POST /prompt` 固定回 retryable 409（`composer_busy`）。
 *
 * 要驗的是「幀來幾次不等於送幾次」，所以輸出的是**觀察到第一次 POST 之後再等一小段**的計數：
 * 舊行為那 6 幀各自排一個 350ms 的 timer，會在幾十毫秒內擠出 6 次；新行為在這段視窗裡只會有 1 次
 * （下一次要等退避的 1 秒）。輸入 `AM_FLUSH_FRAMES`（幾幀）、`AM_FLUSH_GRACE_MS`（視窗）。
 */
const g = globalThis as unknown as Record<string, unknown>
const stored: Record<string, string> = {}
const noop = () => {}
g.localStorage = {
  getItem: (k: string) => (k in stored ? stored[k] : null),
  setItem: (k: string, v: string) => {
    stored[k] = String(v)
  },
  removeItem: (k: string) => {
    delete stored[k]
  },
}
g.document = { visibilityState: 'hidden', hasFocus: () => false, addEventListener: noop, removeEventListener: noop }
g.window = { addEventListener: noop, removeEventListener: noop }
g.location = { protocol: 'http:', host: '127.0.0.1:7788' }

const FRAMES = Number(process.env.AM_FLUSH_FRAMES ?? 6)
const GRACE_MS = Number(process.env.AM_FLUSH_GRACE_MS ?? 300)
/** `cap`：退避換成 10ms × 6，一路送到上限為止（真的退避表要跑 61 秒）。 */
const MODE = process.env.AM_FLUSH_MODE ?? 'window'

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const prompts: string[] = []
const run = { id: 'r1', state: 'running', agent_status: 'idle' }
const state = { daemon_seq: 1, projects: [{ id: 'P1', path: '/p1', bots: [{ id: 'B1', name: 'B1', kind: 'claude', run }] }] }

g.fetch = async (input: string, init?: { method?: string; body?: string }) => {
  const path = String(input).split('?')[0]
  if (path === '/api/session') return json({ token: 't' })
  if (path === '/api/state') return json(state)
  if (path === '/api/bots/B1/prompt') {
    prompts.push(String((JSON.parse(init?.body ?? '{}') as { client_request_id?: string }).client_request_id))
    // API.md §5 的 retryable 409：字沒進去、連 turn 都不建。
    return json({ reason: 'composer_busy', retryable: true, sent: false, run_id: 'r1' }, 409)
  }
  if (/^\/api\/bots\/[^/]+\/messages$/.test(path)) return json({ messages: [], turns: [], has_more: false })
  return json({ error: 'not_found' }, 404)
}

class FakeSocket {
  static last: FakeSocket | null = null
  readyState = 0
  onopen: (() => void) | null = null
  onmessage: ((ev: { data: string }) => void) | null = null
  onerror: (() => void) | null = null
  onclose: (() => void) | null = null
  constructor() {
    FakeSocket.last = this
  }
  close() {
    this.readyState = 3
  }
}
g.WebSocket = FakeSocket

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))
/** 等某件事成立；逾時就讓斷言拿到當下的數字去紅，不要自己 throw 掩蓋原因。 */
async function until(ok: () => boolean, ms: number) {
  const end = Date.now() + ms
  while (!ok() && Date.now() < end) await sleep(10)
}

const { setFlushBackoffForTest, flushMaxAttempts } = await import('./flushRetry.ts')
if (MODE === 'cap') setFlushBackoffForTest([10, 10, 10, 10, 10, 10])

const { useStore } = await import('./store.ts')
await useStore.getState().bootstrap()
await sleep(30)
const sock = FakeSocket.last
if (!sock) throw new Error('store never opened a socket')
sock.readyState = 1
sock.onopen?.()
await sleep(30)

// 回合中按 Enter：訊息進佇列。
useStore.getState().queueSend('B1', '排隊的那一則', [])

// daemon 連續推的 bot_status（只有 status_line 變，agent_status 一直是 idle）。
for (let i = 0; i < FRAMES; i++) {
  sock.onmessage?.({
    data: JSON.stringify({
      seq: 10 + i,
      type: 'bot_status',
      data: { bot_id: 'B1', host: 'local', connected: true, run: { ...run, status_line: `5h:85%(rst 3h 4${i}m)` } },
    }),
  })
  await sleep(40)
}

if (MODE === 'cap') {
  // 一路撞 409 到停手為止：不該無限重試，而且「放棄」只講一次。
  await until(() => prompts.length >= flushMaxAttempts(), 10_000)
  await sleep(300)
  const atCap = prompts.length
  const gaveUp = (): number => useStore.getState().notices.filter((n) => n.text.includes('先停下來了')).length
  const gaveUpNotices = gaveUp()
  // 把 toast 關掉再推幀：不然「只講一次」會被 `notify` 的去重蓋過去，測不到放棄旗標本身。
  for (const n of useStore.getState().notices) useStore.getState().dismiss(n.id)
  // 停手之後 daemon 照樣會推 bot_status：不可以因此又開始送，也不可以再講一次。
  for (let i = 0; i < 3; i++) {
    sock.onmessage?.({
      data: JSON.stringify({
        seq: 100 + i,
        type: 'bot_status',
        data: { bot_id: 'B1', host: 'local', connected: true, run: { ...run, status_line: `after-cap-${i}` } },
      }),
    })
    await sleep(60)
  }
  await sleep(200)
  const s2 = useStore.getState()
  console.log(
    JSON.stringify({
      attempts: atCap,
      afterMoreFrames: prompts.length,
      max: flushMaxAttempts(),
      gaveUpNotices,
      gaveUpAfterDismiss: gaveUp(),
      notices: s2.notices.length,
      stillQueued: Boolean(s2.queuedSends.B1),
    }),
  )
  process.exit(0)
}

// 第一次送出（350ms 防抖）之後再看一小段：舊行為的 6 個 timer 就是擠在這裡。
await until(() => prompts.length > 0, 5_000)
await sleep(GRACE_MS)
const inWindow = prompts.length
const noticesInWindow = useStore.getState().notices.length

// 再等到退避的下一次，確認 toast 沒有疊第二張（同一句 `notify` 去重）。
await until(() => prompts.length > inWindow, 5_000)
const s = useStore.getState()
console.log(
  JSON.stringify({
    frames: FRAMES,
    inWindow,
    noticesInWindow,
    afterRetry: prompts.length,
    noticesAfterRetry: s.notices.length,
    distinctTexts: new Set(s.notices.map((n) => n.text)).size,
    stillQueued: Boolean(s.queuedSends.B1),
  }),
)
process.exit(0)
