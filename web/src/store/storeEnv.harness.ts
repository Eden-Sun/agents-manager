
export interface FakeRequest {
  method: string
  path: string
  body: unknown
}

export const requests: FakeRequest[] = []

const ok = (body: unknown = {}) => new Response(JSON.stringify(body), { status: 200 })
let route: (req: FakeRequest) => Response | Promise<Response> = () => ok()

export function routeDaemon(handler: (req: FakeRequest) => Response | Promise<Response>): void {
  route = handler
}

export function reset(): void {
  const { resetStoreForTest } = require('./store.ts') as typeof import('./store.ts')
  resetStoreForTest()
  requests.length = 0
  route = () => ok()
  g.fetch = fakeFetch
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
g.document = { visibilityState: 'hidden', hasFocus: () => false, addEventListener: noop, removeEventListener: noop }
g.window = { addEventListener: noop, removeEventListener: noop }
g.location = { protocol: 'http:', host: '127.0.0.1:7788' }
const fakeFetch = async (input: string, init?: { method?: string; body?: string }) => {
  const req: FakeRequest = {
    method: init?.method ?? 'GET',
    path: String(input),
    body: init?.body ? (JSON.parse(init.body) as unknown) : undefined,
  }
  requests.push(req)
  return route(req)
}
g.fetch = fakeFetch
