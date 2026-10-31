/**
 * #1802：整頁載入（`loadMessages`／`loadGroupMessages`）的競態——
 * 逆序回應時較早發出的舊頁不能蓋掉較新請求的整頁；請求在途時新排進的 queued 回合不能被整頁快照刪掉；
 * 本地已到終態的回合不能被舊頁蓋回 in_flight（#649）。回應順序由測試自己決定，不靠 sleep。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon, type FakeRequest } from './storeEnv.harness.ts'

const { useStore } = await import('./store.ts')

const page = (content: string, turns: unknown[] = []) =>
  new Response(
    JSON.stringify({
      messages: [{ id: 'm1', conversation_id: 'c1', bot_id: 'b1', role: 'assistant', content, created_at: '2026-01-01T00:00:00.000Z' }],
      turns,
      has_more: false,
    }),
    { status: 200 },
  )

const turn = (id: string, status: string, created_at: string) => ({
  id,
  conversation_id: 'c1',
  bot_id: 'b1',
  run_id: null,
  origin: 'web',
  status,
  delivery: 'pending',
  unverified: false,
  autoResend: true,
  awaitsStart: false,
  awaitsIdle: status === 'queued',
  startError: null,
  client_request_id: null,
  created_at,
  completed_at: null,
})

/** 指定路徑的 GET 先全部掛起；回傳的陣列依發出順序收集 resolver，由測試決定哪一頁先回。 */
function holdPages(path: string): Array<(r: Response) => void> {
  const waiting: Array<(r: Response) => void> = []
  routeDaemon((r: FakeRequest) =>
    r.path.includes(path)
      ? new Promise<Response>((resolve) => waiting.push(resolve))
      : new Response('{}', { status: 200 }),
  )
  return waiting
}

test('逆序回應：較早發出的舊頁晚到，不蓋掉較新請求的整頁', async () => {
  reset()
  const waiting = holdPages('/bots/b1/messages')
  useStore.setState({ messages: {}, turns: {}, loadedBots: {} })
  const older = useStore.getState().loadMessages('b1')
  const newer = useStore.getState().loadMessages('b1')
  assert.equal(waiting.length, 2)
  waiting[1](page('new transcript'))
  await newer
  waiting[0](page('old fallback'))
  await older
  assert.equal(useStore.getState().messages.b1?.[0]?.content, 'new transcript')
  assert.equal(useStore.getState().loadedBots.b1, true, '新頁已經到了，對話要算載入完成')
})

test('整頁快照：請求在途時新排進的 queued 回合保留；快照之前就已不在的舊 queued 回合移除', async () => {
  reset()
  const waiting = holdPages('/bots/b1/messages')
  useStore.setState({
    messages: {},
    turns: { b1: { stale: turn('stale', 'queued', '2000-01-01T00:00:00.000Z') as never } },
    loadedBots: {},
  })
  const load = useStore.getState().loadMessages('b1')
  assert.equal(waiting.length, 1)
  useStore.setState((s) => ({
    turns: { b1: { ...s.turns.b1, newq: turn('newq', 'queued', '2999-01-01T00:00:00.000Z') as never } },
  }))
  waiting[0](page('snapshot before enqueue'))
  await load
  const turns = useStore.getState().turns.b1 ?? {}
  assert.equal(turns.newq?.status, 'queued', '在途時新排的 queued 回合不能被整頁刪掉')
  assert.equal(turns.stale, undefined, '快照之前就不見的舊 queued 回合要移除')
})

test('整頁快照不把本地已到終態的回合蓋回 in_flight（#649）', async () => {
  reset()
  const waiting = holdPages('/bots/b1/messages')
  useStore.setState({ messages: {}, turns: {}, loadedBots: {} })
  const load = useStore.getState().loadMessages('b1')
  assert.equal(waiting.length, 1)
  useStore.setState({ turns: { b1: { t1: turn('t1', 'completed', '2999-01-01T00:00:00.000Z') as never } } })
  waiting[0](page('snapshot', [turn('t1', 'in_flight', '2999-01-01T00:00:00.000Z')]))
  await load
  assert.equal(useStore.getState().turns.b1?.t1?.status, 'completed')
})

const failure = () => new Response('{"error":"boom"}', { status: 500 })
const loadFailedNotices = () => useStore.getState().notices.filter((n) => /載入(群組)?訊息失敗/.test(n.text))

test('舊請求晚到失敗，新請求已成功：不設載入失敗、不跳錯誤通知', async () => {
  reset()
  const waiting = holdPages('/bots/b1/messages')
  useStore.setState({ messages: {}, turns: {}, loadedBots: {}, messageLoadFailed: {}, notices: [] })
  const older = useStore.getState().loadMessages('b1')
  const newer = useStore.getState().loadMessages('b1')
  waiting[1](page('new transcript'))
  await newer
  waiting[0](failure())
  await older
  const s = useStore.getState()
  assert.equal(s.loadedBots.b1, true)
  assert.equal(s.messageLoadFailed.b1, undefined, '舊請求的失敗不能把已載入的對話換成重試鈕')
  assert.equal(loadFailedNotices().length, 0, '舊請求的失敗不能跳「載入訊息失敗」')
})

test('新請求失敗，舊請求成功晚到：對話保持未載入，畫面給重試鈕', async () => {
  reset()
  const waiting = holdPages('/bots/b1/messages')
  useStore.setState({ messages: {}, turns: {}, loadedBots: {}, messageLoadFailed: {}, notices: [] })
  const older = useStore.getState().loadMessages('b1')
  const newer = useStore.getState().loadMessages('b1')
  waiting[1](failure())
  await newer
  waiting[0](page('old fallback'))
  await older
  const s = useStore.getState()
  assert.notEqual(s.loadedBots.b1, true, '舊頁不能把對話算成已載入')
  assert.equal(s.messageLoadFailed.b1, true, '新請求失敗要給重試鈕')
  assert.equal(loadFailedNotices().length, 1, '新請求的失敗要跳通知')
})
