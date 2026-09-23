import test from 'node:test'
import assert from 'node:assert/strict'
import { setOrderSaveTimeoutForTest, useStore } from './store.ts'

type Bot = ReturnType<typeof useStore.getState>['bots'][number]
const bot = (id: string, position: number) => ({ ...({} as Bot), id, project_id: 'p1', primary: true, primary_position: position })

function stubFetch(status: number, body: unknown) {
  const calls: { url: string; method: string; body: unknown }[] = []
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    calls.push({ url: String(input), method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined })
    return {
      ok: status >= 200 && status < 300,
      status,
      statusText: String(status),
      text: async () => JSON.stringify(body),
    } as unknown as Response
  }) as unknown as typeof fetch
  return calls
}

const settle = () => new Promise((r) => setTimeout(r, 20))
const positions = () => Object.fromEntries(useStore.getState().bots.map((b) => [b.id, b.primary_position]))

/** 2026-09-20 使用者：拖了順序沒存起來，重整後回到原本的順序。放開就要 POST /api/order {primary}，整份順序。 */
test('拖完立刻把整份主力順序送給 daemon，畫面先套用', async () => {
  useStore.setState({ bots: [bot('a', 0), bot('b', 0), bot('c', 0)] })
  const calls = stubFetch(200, { ok: true })
  useStore.getState().movePrimary(['c', 'a', 'b'])
  assert.deepEqual(positions(), { c: 0, a: 1, b: 2 }, '樂觀套用')
  await settle()
  const post = calls.find((c) => c.url === '/api/order')
  assert.ok(post, `沒有送出：${JSON.stringify(calls)}`)
  assert.equal(post.method, 'POST')
  assert.deepEqual(post.body, { primary: ['c', 'a', 'b'] })
  assert.deepEqual(positions(), { c: 0, a: 1, b: 2 }, '成功就維持')
})

test('daemon 拒絕（舊版不認得 primary → 400）：回捲，並把原因講出來，不是只說「沒收到」', async () => {
  useStore.setState({ bots: [bot('a', 1), bot('b', 2), bot('c', 3)], notices: [] })
  stubFetch(400, { error: 'bad_request', message: 'order: projects 或 bots 至少要有一個' })
  useStore.getState().movePrimary(['c', 'a', 'b'])
  await settle()
  assert.deepEqual(positions(), { a: 1, b: 2, c: 3 }, '失敗回到原本的位置')
  const notice = useStore.getState().notices.find((n) => n.kind === 'error')
  assert.ok(notice, '要跳錯誤通知')
  assert.match(notice.text, /主力順序沒存起來/)
  assert.match(notice.text, /400|至少要有一個/, `通知要帶 daemon 的原因：${notice.text}`)
})

/**
 * 模擬 daemon：每個 `/api/order` POST 在「抵達」時落庫；`delays[i]` 是第 i 個請求在路上走多久，
 * `fail[i]` 為真就回 502 不落庫。回傳 daemon 目前存的主力順序與收到的請求序。
 */
function stubDaemon(delays: number[], fail: boolean[] = []) {
  const server = { order: null as string[] | null, arrived: [] as string[][], signals: [] as (AbortSignal | null)[] }
  let n = 0
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const i = n++
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    if (String(input) === '/api/order') {
      server.signals.push(init?.signal ?? null)
      const signal = init?.signal
      await new Promise((resolve, reject) => {
        // Infinity＝daemon 永遠不回；跟真的 fetch 一樣，只有 signal 中止時才會 reject。
        if (delays[i] !== Infinity) setTimeout(resolve, delays[i] ?? 0)
        signal?.addEventListener('abort', () => reject(signal.reason))
      })
      if (fail[i]) return { ok: false, status: 502, statusText: '502', text: async () => JSON.stringify({ error: 'upstream', message: 'daemon rebuilding' }) } as unknown as Response
      server.order = body.primary
      server.arrived.push(body.primary)
    }
    return { ok: true, status: 200, statusText: '200', text: async () => '{"ok":true}' } as unknown as Response
  }) as unknown as typeof fetch
  return server
}

/** #391：快速拖兩次，第一個 POST 在路上慢、第二個先到——daemon 最後不能是舊順序。 */
test('兩次主力排序請求亂序完成時，較新的順序不能被舊請求覆蓋', async () => {
  useStore.setState({ bots: [bot('a', 0), bot('b', 1), bot('c', 2)], notices: [] })
  const server = stubDaemon([30, 0])
  useStore.getState().movePrimary(['c', 'a', 'b'])
  useStore.getState().movePrimary(['b', 'c', 'a'])
  await new Promise((r) => setTimeout(r, 80))
  assert.deepEqual(server.order, ['b', 'c', 'a'], `daemon 最後要是較新的順序；抵達序 ${JSON.stringify(server.arrived)}`)
  assert.deepEqual(positions(), { b: 0, c: 1, a: 2 }, '畫面維持較新的順序')
  assert.equal(useStore.getState().notices.filter((x) => x.kind === 'error').length, 0, '兩次都成功，不該有錯誤通知')
})

/** #275 在主力上的同一件事：舊的晚到失敗，不能把已存成功的新順序退回去。 */
test('兩次主力排序、較舊的失敗而較新的成功：不回捲、不跳錯誤', async () => {
  useStore.setState({ bots: [bot('a', 0), bot('b', 1), bot('c', 2)], notices: [] })
  const server = stubDaemon([30, 0], [true, false])
  useStore.getState().movePrimary(['c', 'a', 'b'])
  useStore.getState().movePrimary(['b', 'c', 'a'])
  await new Promise((r) => setTimeout(r, 80))
  assert.deepEqual(server.order, ['b', 'c', 'a'])
  assert.deepEqual(positions(), { b: 0, c: 1, a: 2 }, '較新的樂觀順序已經落庫，不能被舊失敗回捲')
  assert.equal(useStore.getState().notices.filter((x) => x.kind === 'error').length, 0)
})

test('兩次主力排序、較舊的成功而較新的失敗：回到 daemon 存下的那一份，並講原因', async () => {
  useStore.setState({ bots: [bot('a', 0), bot('b', 1), bot('c', 2)], notices: [] })
  const server = stubDaemon([30, 0], [false, true])
  useStore.getState().movePrimary(['c', 'a', 'b'])
  useStore.getState().movePrimary(['b', 'c', 'a'])
  await new Promise((r) => setTimeout(r, 80))
  assert.deepEqual(server.order, ['c', 'a', 'b'])
  assert.deepEqual(positions(), { c: 0, a: 1, b: 2 }, '畫面要跟 daemon 存的一致')
  const notice = useStore.getState().notices.find((x) => x.kind === 'error')
  assert.ok(notice, '要跳錯誤通知')
  assert.match(notice.text, /主力順序沒存起來.*daemon rebuilding/)
})

/** #391 追加：transport 沒有逾時，前一個 POST 永遠不回的話，同範圍後面的存檔不能跟著永遠排隊。 */
test('前一個主力排序 POST 永遠不回：逾時後中止它、送出下一個，最後是新的順序', async () => {
  setOrderSaveTimeoutForTest(40)
  try {
    useStore.setState({ bots: [bot('a', 0), bot('b', 1), bot('c', 2)], notices: [] })
    const server = stubDaemon([Infinity, 0])
    useStore.getState().movePrimary(['c', 'a', 'b'])
    useStore.getState().movePrimary(['b', 'c', 'a'])
    await new Promise((r) => setTimeout(r, 150))
    assert.deepEqual(server.order, ['b', 'c', 'a'], `逾時後要送出下一個；抵達序 ${JSON.stringify(server.arrived)}`)
    assert.equal(server.signals[0]?.aborted, true, '卡住的那個要真的中止，不然它晚到一樣會蓋掉新順序')
    assert.deepEqual(positions(), { b: 0, c: 1, a: 2 })
    assert.equal(useStore.getState().notices.filter((x) => x.kind === 'error').length, 0, '較舊的逾時被較新的成功取代，不跳錯誤')
  } finally {
    setOrderSaveTimeoutForTest(15_000)
  }
})

test('只有一個主力排序 POST 且永遠不回：逾時當失敗，回捲並說是逾時', async () => {
  setOrderSaveTimeoutForTest(40)
  try {
    useStore.setState({ bots: [bot('a', 0), bot('b', 1), bot('c', 2)], notices: [] })
    stubDaemon([Infinity])
    useStore.getState().movePrimary(['c', 'a', 'b'])
    await new Promise((r) => setTimeout(r, 120))
    assert.deepEqual(positions(), { a: 0, b: 1, c: 2 }, '逾時＝沒存起來，要回到原本的順序')
    const notice = useStore.getState().notices.find((x) => x.kind === 'error')
    assert.ok(notice, '要跳錯誤通知')
    assert.match(notice.text, /主力順序沒存起來.*逾時/)
  } finally {
    setOrderSaveTimeoutForTest(15_000)
  }
})
