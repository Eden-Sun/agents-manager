/**
 * `storeActions.test.ts` 的跑道：有些不變量只在**真的按下去**時才成立（送失敗要收回、已讀要守住），
 * 純函式測不到，所以這裡把 store.ts 會碰到的瀏覽器物件補上，讓測試直接操作 `useStore`。
 *
 * daemon 由 `routeDaemon()` 扮演：一個行程只 import 一次 store.ts，測試之間用 `reset()` 換掉路由與紀錄。
 */

export interface FakeRequest {
  method: string
  path: string
  body: unknown
}

/** 送出去的每一筆請求，測試用來核對 body（例如重送有沒有沿用同一個 crid）。 */
export const requests: FakeRequest[] = []

const ok = (body: unknown = {}) => new Response(JSON.stringify(body), { status: 200 })
let route: (req: FakeRequest) => Response = () => ok()

/** 這一段測試裡 daemon 怎麼回；回傳 `Response`，非 2xx 會被 transport 包成 `ApiError`。 */
export function routeDaemon(handler: (req: FakeRequest) => Response): void {
  route = handler
}

export function reset(): void {
  requests.length = 0
  route = () => ok()
}

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
// 背景分頁：沒在看，未讀不會被「正在看」的豁免蓋過去。
g.document = { visibilityState: 'hidden', hasFocus: () => false, addEventListener: noop, removeEventListener: noop }
g.window = { addEventListener: noop, removeEventListener: noop }
g.location = { protocol: 'http:', host: '127.0.0.1:7788' }
g.fetch = async (input: string, init?: { method?: string; body?: string }) => {
  const req: FakeRequest = {
    method: init?.method ?? 'GET',
    path: String(input),
    body: init?.body ? (JSON.parse(init.body) as unknown) : undefined,
  }
  requests.push(req)
  return route(req)
}
