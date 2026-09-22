import test from 'node:test'
import assert from 'node:assert/strict'
import { useStore } from './store.ts'

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

test('兩次主力排序請求亂序完成時，較新的順序不能被舊請求覆蓋（#391）', async () => {
  useStore.setState({ bots: [bot('a', 0), bot('b', 1), bot('c', 2)], notices: [] })
  const calls: { url: string; method: string; body: unknown }[] = []
  let serverOrder = ['a', 'b', 'c']
  let releaseFirst!: () => void
  let firstStarted!: () => void
  const firstCallStarted = new Promise<void>((resolve) => {
    firstStarted = resolve
  })
  const firstResponse = new Promise<void>((resolve) => {
    releaseFirst = resolve
  })
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    calls.push({ url: String(input), method: init?.method ?? 'GET', body })
    if (calls.length === 1) {
      firstStarted()
      await firstResponse
    }
    serverOrder = (body as { primary: string[] }).primary
    return {
      ok: true,
      status: 200,
      statusText: 'OK',
      text: async () => JSON.stringify({ ok: true }),
    } as unknown as Response
  }) as unknown as typeof fetch

  useStore.getState().movePrimary(['c', 'a', 'b'])
  useStore.getState().movePrimary(['b', 'c', 'a'])
  await firstCallStarted
  assert.equal(calls.length, 1, '第二筆必須等第一筆 POST 回應後才開始')

  releaseFirst()
  await settle()
  assert.deepEqual(
    calls.map((c) => c.body),
    [{ primary: ['c', 'a', 'b'] }, { primary: ['b', 'c', 'a'] }],
    '同一排序範圍的 POST 要照拖曳順序送出',
  )
  assert.deepEqual(serverOrder, ['b', 'c', 'a'], '最後落庫的必須是較新的順序')
  assert.deepEqual(positions(), { a: 2, b: 0, c: 1 }, '畫面也維持最後一次拖曳')
})
