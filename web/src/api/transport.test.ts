import test from 'node:test'
import assert from 'node:assert/strict'
import { HttpTransport, pairWithCode } from './transport.ts'
import { memoryTokenStore } from './sessionToken.ts'
import { ApiError } from './types.ts'

interface Call {
  url: string
  method: string
  token: string | null
  body: string | null
}

/** 依序吐回預先排好的回應；排完了就一律 200 `{}`（免得測試自己爆在 undefined 上）。 */
function stubFetch(replies: { status: number; body: unknown }[]): Call[] {
  const calls: Call[] = []
  let i = 0
  const fake = (input: unknown, init?: { method?: string; headers?: Record<string, string>; body?: unknown }) => {
    const headers = init?.headers ?? {}
    calls.push({
      url: String(input),
      method: init?.method ?? 'GET',
      token: headers['X-AM-Token'] ?? null,
      body: typeof init?.body === 'string' ? init.body : null,
    })
    const reply = replies[i] ?? { status: 200, body: {} }
    i += 1
    return Promise.resolve(
      new Response(JSON.stringify(reply.body), {
        status: reply.status,
        headers: { 'Content-Type': 'application/json' },
      }),
    )
  }
  ;(globalThis as unknown as { fetch: unknown }).fetch = fake
  return calls
}

test('裝置上存過的 token 直接拿來用，不再問 /api/session', async () => {
  const calls = stubFetch([{ status: 200, body: { daemon_seq: 1 } }])
  const t = new HttpTransport(memoryTokenStore('saved-token'))

  assert.equal(await t.session(), 'saved-token')
  // 這是整條規則的重點：非 loopback 問 /api/session 只會拿到 403，配過一次就不該再問。
  assert.equal(calls.length, 0)

  await t.request('GET', '/state')
  assert.equal(calls.length, 1)
  assert.equal(calls[0].token, 'saved-token')
})

test('沒存過 token 才打 /api/session，拿到就存起來', async () => {
  const store = memoryTokenStore()
  const calls = stubFetch([{ status: 200, body: { token: 'fresh', port: 7788 } }])
  const t = new HttpTransport(store)

  assert.equal(await t.session(), 'fresh')
  assert.equal(calls[0].url, '/api/session')
  assert.equal(store.get(), 'fresh')
})

test('沒配對過的裝置：/api/session 的 403 pairing_required 原樣往上丟', async () => {
  stubFetch([{ status: 403, body: { error: 'pairing_required' } }])
  const t = new HttpTransport(memoryTokenStore())

  const e = await t.session().then(
    () => null,
    (err: unknown) => err,
  )
  assert.ok(e instanceof ApiError)
  assert.equal(e.status, 403)
  assert.equal(e.body.error, 'pairing_required')
})

test('請求收到 401 就清掉那把 token、重新取得一把再送一次', async () => {
  const store = memoryTokenStore('stale')
  const calls = stubFetch([
    { status: 401, body: { error: 'missing or bad X-AM-Token' } },
    { status: 200, body: { token: 'fresh', port: 7788 } },
    { status: 200, body: { daemon_seq: 9 } },
  ])
  const t = new HttpTransport(store)

  const got = await t.request('GET', '/state')
  assert.deepEqual(got, { daemon_seq: 9 })
  assert.deepEqual(
    calls.map((c) => c.url),
    ['/api/state', '/api/session', '/api/state'],
  )
  // 舊的那把不能留在裝置上，否則下次開機又是一輪 401。
  assert.equal(store.get(), 'fresh')
  assert.equal(calls[2].token, 'fresh')
})

test('重新取得的 token 又被打回 401 就認輸，不會一直繞', async () => {
  const store = memoryTokenStore('stale')
  const calls = stubFetch([
    { status: 401, body: { error: 'bad token' } },
    { status: 200, body: { token: 'fresh', port: 7788 } },
    { status: 401, body: { error: 'bad token' } },
  ])
  const t = new HttpTransport(store)

  const e = await t.request('GET', '/state').then(
    () => null,
    (err: unknown) => err,
  )
  assert.ok(e instanceof ApiError)
  assert.equal(e.status, 401)
  assert.equal(calls.length, 3)
})

test('401 之後又被要求配對：清掉 token 並通知一次，回到配對畫面', async () => {
  const store = memoryTokenStore('stale')
  stubFetch([
    { status: 401, body: { error: 'bad token' } },
    { status: 403, body: { error: 'pairing_required' } },
  ])
  const t = new HttpTransport(store)
  let told = 0
  t.setPairingListener(() => {
    told += 1
  })

  const e = await t.request('GET', '/state').then(
    () => null,
    (err: unknown) => err,
  )
  assert.ok(e instanceof ApiError)
  assert.equal(e.status, 401)
  assert.equal(told, 1)
  assert.equal(store.get(), '')
})

test('同時多個請求吃到 401，只重新取得一次 token', async () => {
  const store = memoryTokenStore('stale')
  const calls = stubFetch([
    { status: 401, body: {} },
    { status: 401, body: {} },
    { status: 200, body: { token: 'fresh', port: 7788 } },
    { status: 200, body: { a: 1 } },
    { status: 200, body: { b: 2 } },
  ])
  const t = new HttpTransport(store)

  await Promise.all([t.request('GET', '/state'), t.request('GET', '/quota')])
  assert.equal(calls.filter((c) => c.url === '/api/session').length, 1)
})

test('配對成功就把 token 存進裝置，後續請求都帶著它', async () => {
  const store = memoryTokenStore()
  const calls = stubFetch([
    { status: 200, body: { token: 'paired-token', port: 7788 } },
    { status: 200, body: { daemon_seq: 1 } },
  ])
  const t = new HttpTransport(store)

  await pairWithCode(t, ' abc-def ')
  assert.equal(calls[0].url, '/api/session/pair')
  // 送出去之前先正規化：使用者照唸照打，大小寫與連字號都不算。
  assert.deepEqual(JSON.parse(calls[0].body ?? '{}'), { code: 'ABCDEF' })
  assert.equal(store.get(), 'paired-token')

  await t.request('GET', '/state')
  assert.equal(calls[1].token, 'paired-token')
})

test('配對失敗不會動到裝置上的 token', async () => {
  const store = memoryTokenStore()
  stubFetch([{ status: 403, body: { error: 'pairing_failed' } }])
  const t = new HttpTransport(store)

  const e = await pairWithCode(t, 'WRONG1').then(
    () => null,
    (err: unknown) => err,
  )
  assert.ok(e instanceof ApiError)
  assert.equal(e.body.error, 'pairing_failed')
  assert.equal(store.get(), '')
})
