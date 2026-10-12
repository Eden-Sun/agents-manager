import { test } from 'node:test'
import assert from 'node:assert/strict'
import { HttpTransport } from './transport'
import { ApiError } from './types'

type Call = { url: string; token: string | undefined; method: string }

function stubFetch(handler: (c: Call) => { status: number; body: unknown }) {
  const calls: Call[] = []
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const headers = (init?.headers ?? {}) as Record<string, string>
    const call = { url: String(input), token: headers['X-AM-Token'], method: init?.method ?? 'GET' }
    calls.push(call)
    const { status, body } = handler(call)
    return {
      ok: status >= 200 && status < 300,
      status,
      statusText: String(status),
      text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
      blob: async () => body as Blob,
    } as unknown as Response
  }) as unknown as typeof fetch
  return calls
}

/** 2026-09-20 使用者：blocked 面板卡在「讀取終端失敗：missing or bad X-AM-Token」，重整才好。 */
test('GET 撞 401 會重拿一次 token 再送一次', async () => {
  const t = new HttpTransport()
  const calls = stubFetch((c) => {
    if (c.url === '/api/session') return { status: 200, body: { token: 'good' } }
    return c.token === 'good' ? { status: 200, body: { ok: true } } : { status: 401, body: { error: 'missing or bad X-AM-Token' } }
  })
  assert.deepEqual(await t.request('GET', '/bots/b1/terminal'), { ok: true })
  assert.deepEqual(
    calls.map((c) => c.url),
    ['/api/bots/b1/terminal', '/api/session', '/api/bots/b1/terminal'],
  )
  // 拿到之後的請求直接帶著新 token，不用每次都補一趟 session。
  await t.request('GET', '/state')
  assert.equal(calls[calls.length - 1].token, 'good')
  assert.equal(calls.filter((c) => c.url === '/api/session').length, 1)
})

test('重拿 token 之後還是 401 就照實丟 ApiError，不會一直重試', async () => {
  const t = new HttpTransport()
  const calls = stubFetch((c) =>
    c.url === '/api/session' ? { status: 200, body: { token: 'stale' } } : { status: 401, body: { error: 'missing or bad X-AM-Token' } },
  )
  await assert.rejects(() => t.request('GET', '/state'), (e: unknown) => e instanceof ApiError && e.status === 401)
  // 第一次（空 token）→ session → 帶 stale 再一次；就這三通。
  assert.equal(calls.length, 3)
})

test('session 本身掛掉時不吞錯：照樣回報原本的 401', async () => {
  const t = new HttpTransport()
  stubFetch((c) => (c.url === '/api/session' ? { status: 500, body: 'boom' } : { status: 401, body: { error: 'nope' } }))
  await assert.rejects(() => t.request('GET', '/state'), (e: unknown) => e instanceof ApiError && e.status === 401)
})

test('寫入請求不重送：401 也只送一次（重送可能變成送兩次）', async () => {
  const t = new HttpTransport()
  const calls = stubFetch(() => ({ status: 401, body: { error: 'nope' } }))
  await assert.rejects(() => t.request('POST', '/bots/b1/prompt', { text: 'hi' }))
  assert.equal(calls.length, 1)
})

/** 假的計時器：`fireAll` 手動觸發，不靠牆鐘。 */
function fakeTimers() {
  let next = 1
  const live = new Map<number, () => void>()
  return {
    setTimeout: (fn: () => void) => {
      const id = next++
      live.set(id, fn)
      return id
    },
    clearTimeout: (id: unknown) => void live.delete(id as number),
    pending: () => live.size,
    fireAll: () => {
      for (const [id, fn] of [...live]) {
        live.delete(id)
        fn()
      }
    },
  }
}

test('GET 掛住不回：到時候要中止並丟錯，不能永遠等（否則 singleFlight 的 refreshState 會永遠卡在這一趟）', async () => {
  const timers = fakeTimers()
  const t = new HttpTransport({ timers })
  globalThis.fetch = ((_: string, init?: RequestInit) =>
    new Promise((_resolve, reject) => {
      init?.signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')))
    })) as unknown as typeof fetch
  const pending = t.request('GET', '/state')
  timers.fireAll()
  await assert.rejects(pending, (e: unknown) => e instanceof Error && /逾時/.test(e.message))
  assert.equal(timers.pending(), 0)
})

test('GET 很快就回來：逾時計時器要清掉，不留著', async () => {
  const timers = fakeTimers()
  const t = new HttpTransport({ timers })
  stubFetch(() => ({ status: 200, body: { ok: true } }))
  assert.deepEqual(await t.request('GET', '/state'), { ok: true })
  assert.equal(timers.pending(), 0)
})

