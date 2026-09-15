/**
 * `groupUnread.test.ts` 的整合測試跑道：一個行程＝一次開機（重整／新分頁）。
 *
 * 真的 import `store.ts`、跑 `bootstrap()`，daemon 由假 fetch／WebSocket 扮演；localStorage 存在
 * `AM_HARNESS_STORAGE` 指的 JSON 檔，下一個行程讀回來＝重整後。模組層的記憶體（群組 turn 的 Set、
 * 完成去重）每次開機都是空的，跟真的重整一樣。
 *
 * 輸入 `AM_HARNESS_SCENARIO`（JSON）：`state` 是 `/api/state` 的回應、`frames` 是連上後依序推的 WS frame、
 * `messages` 是 `/api/bots/:id/messages` 的回應。輸出一行 JSON：`groupUnread`、`botUnread`、`fetches`（抓過哪些 bot 的訊息）。
 */
import { readFileSync, writeFileSync } from 'node:fs'

interface Scenario {
  state: unknown
  frames: unknown[]
  messages?: Record<string, unknown>
}

const storagePath = process.env.AM_HARNESS_STORAGE ?? ''
const scenario = JSON.parse(process.env.AM_HARNESS_SCENARIO ?? '{}') as Scenario

let stored: Record<string, string> = {}
try {
  stored = JSON.parse(readFileSync(storagePath, 'utf8')) as Record<string, string>
} catch {
  stored = {}
}

const g = globalThis as unknown as Record<string, unknown>
g.localStorage = {
  getItem: (k: string) => (k in stored ? stored[k] : null),
  setItem: (k: string, v: string) => {
    stored[k] = String(v)
  },
  removeItem: (k: string) => {
    delete stored[k]
  },
}
const noop = () => {}
// 背景分頁：沒在看，完成的回合要記未讀。
g.document = { visibilityState: 'hidden', hasFocus: () => false, addEventListener: noop, removeEventListener: noop }
g.window = { addEventListener: noop, removeEventListener: noop }
g.location = { protocol: 'http:', host: '127.0.0.1:7788' }

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const fetches: string[] = []
interface PageMsg {
  role?: string
  turn_id?: string | null
}
g.fetch = async (input: string) => {
  const [path, query] = String(input).split('?')
  if (path === '/api/session') return json({ token: 't' })
  if (path === '/api/state') return json(scenario.state)
  const m = /^\/api\/bots\/([^/]+)\/messages$/.exec(path)
  if (m) {
    const botId = decodeURIComponent(m[1])
    fetches.push(botId)
    const page = (scenario.messages?.[botId] ?? { messages: [], turns: [], has_more: false }) as { messages: PageMsg[] }
    // daemon 的 `turn_id` / `role` 過濾（API.md §6）：測試要看到跟真 daemon 一樣的那一頁。
    const q = new URLSearchParams(query ?? '')
    const turnId = q.get('turn_id')
    const role = q.get('role')
    const messages = page.messages.filter((x) => (!turnId || x.turn_id === turnId) && (!role || x.role === role))
    return json({ ...page, messages })
  }
  return json({ error: 'not_found' }, 404)
}

class FakeSocket {
  static CONNECTING = 0
  static OPEN = 1
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

const settle = () => new Promise((r) => setTimeout(r, 20))

const { useStore } = await import('./store.ts')
await useStore.getState().bootstrap()
await settle()
const sock = FakeSocket.last
if (!sock) throw new Error('store never opened a socket')
sock.readyState = 1
sock.onopen?.()
await settle()
for (const frame of scenario.frames) {
  sock.onmessage?.({ data: JSON.stringify(frame) })
  await settle()
}

await settle()
writeFileSync(storagePath, JSON.stringify(stored))
const s = useStore.getState()
console.log(JSON.stringify({ groupUnread: s.groupUnread, botUnread: s.botUnread, bootError: s.bootError, fetches }))
process.exit(0)
