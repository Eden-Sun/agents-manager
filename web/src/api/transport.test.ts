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

test('/api/session 回的是網頁（代理或前端的退回頁、200）：不能拿那串 HTML 當 token', async () => {
  const t = new HttpTransport()
  const html = '<!doctype html><html><body>index</body></html>'
  const sentTokens: (string | undefined)[] = []
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const headers = (init?.headers ?? {}) as Record<string, string>
    if (String(input) === '/api/session') {
      return { ok: true, status: 200, headers: new Headers({ 'content-type': 'text/html' }), text: async () => html } as unknown as Response
    }
    sentTokens.push(headers['X-AM-Token'])
    return { ok: true, status: 200, headers: new Headers(), text: async () => '{}' } as unknown as Response
  }) as unknown as typeof fetch
  await assert.rejects(() => t.session(), (e: unknown) => e instanceof Error && !(e instanceof ApiError))
  await t.request('GET', '/state').catch(() => {})
  // 沒有拿到 token 就不帶 X-AM-Token；HTML 整頁絕不會出現在任何請求的 header 裡。
  assert.ok(sentTokens.every((tok) => tok === undefined || !tok.includes('<')), JSON.stringify(sentTokens))
})

test('代理回的 HTML 錯誤頁：錯誤訊息要是短短一句，不能把整頁 HTML 當成提示顯示', async () => {
  const t = new HttpTransport()
  const page = `<!DOCTYPE html><html><head><title>502 Bad Gateway</title></head><body><h1>Bad Gateway</h1>${'<p>nginx</p>'.repeat(40)}</body></html>`
  stubFetch(() => ({ status: 502, body: page }))
  await assert.rejects(
    () => t.request('GET', '/state'),
    (e: unknown) => e instanceof ApiError && e.status === 502 && !e.message.includes('<') && e.message.length < 80,
  )
})

function jsonRes(status: number, body: unknown): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: new Headers({ 'content-type': 'application/json' }),
    text: async () => JSON.stringify(body),
  } as unknown as Response
}

/**
 * 假的 fetch：`/api/state` 只認 token `good`，其餘 401。`/api/session` 第一次呼叫會掛住（`where` 指定掛在
 * headers 還是 body），直到測試呼叫 `release()`；掛住時不理 signal（最壞情況：代理把連線吊著）。第二次之後立即回 `good`。
 * `started` 讓測試等到真的送出第一次 session 才往下，不靠固定毫秒。
 */
function refreshFetch(where: 'headers' | 'body') {
  let sessionCalls = 0
  let markStarted!: () => void
  const started = new Promise<void>((r) => (markStarted = r))
  let release!: () => void
  const gate = new Promise<void>((r) => (release = r))
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const url = String(input)
    if (url === '/api/session') {
      sessionCalls += 1
      if (sessionCalls > 1) return jsonRes(200, { token: 'good' })
      markStarted()
      if (where === 'headers') {
        await gate
        return jsonRes(200, { token: 'good' })
      }
      return {
        ok: true,
        status: 200,
        headers: new Headers({ 'content-type': 'application/json' }),
        text: async () => {
          await gate
          return JSON.stringify({ token: 'good' })
        },
      } as unknown as Response
    }
    const tok = (init?.headers as Record<string, string> | undefined)?.['X-AM-Token']
    return tok === 'good' ? jsonRes(200, { ok: true }) : jsonRes(401, { error: 'missing or bad X-AM-Token' })
  }) as unknown as typeof fetch
  return { started, release, sessionCalls: () => sessionCalls }
}

/**
 * 等 event loop 轉完一圈再看 `p` 的結果（只清一輪 macrotask，不是等牆鐘）。
 * `'hung'` 表示 `p` 還沒結束：紅測時回報失敗，不讓測試永遠掛住。
 */
async function outcomeAfterTick(p: Promise<unknown>): Promise<unknown> {
  const done = p.then(
    (v) => v,
    (e: unknown) => e,
  )
  return Promise.race([done, new Promise((r) => setTimeout(() => r('hung'), 0))])
}

/** 2026-10-13 #1629：401 後的 token 更新（`GET /api/session`）掛住，GET 會永遠不結束，`refreshing` 也永遠清不掉。 */
for (const where of ['headers', 'body'] as const) {
  test(`token 更新掛住（${where} 不回）：GET 有期限會逾時，之後 refreshing 可重建、下一次 GET 能成功`, async () => {
    const timers = fakeTimers()
    const t = new HttpTransport({ timers })
    const f = refreshFetch(where)
    const first = t.request('GET', '/state')
    const second = t.request('GET', '/state')
    await f.started
    timers.fireAll()
    for (const p of [first, second]) {
      const out = await outcomeAfterTick(p)
      assert.ok(out instanceof Error && /逾時/.test(out.message), String(out))
    }
    assert.deepEqual(await t.request('GET', '/state'), { ok: true })
    assert.equal(f.sessionCalls(), 2)
    assert.equal(timers.pending(), 0)
  })
}

test('呼叫端中止只放掉自己那一趟：共用的 token 更新還在，其他等它的 GET 照樣拿到結果', async () => {
  const timers = fakeTimers()
  const t = new HttpTransport({ timers })
  const f = refreshFetch('headers')
  const ctl = new AbortController()
  const mine = t.request('GET', '/state', undefined, ctl.signal)
  const theirs = t.request('GET', '/state')
  await f.started
  ctl.abort()
  const out = await outcomeAfterTick(mine)
  assert.ok(out instanceof Error && out.name === 'AbortError', String(out))
  f.release()
  assert.deepEqual(await theirs, { ok: true })
  assert.equal(f.sessionCalls(), 1)
  assert.equal(timers.pending(), 0)
})
