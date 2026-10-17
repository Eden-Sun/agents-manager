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

test('群組：逆序回應不蓋掉較新請求的整頁', async () => {
  reset()
  const waiting = holdPages('/projects/p1/messages')
  useStore.setState({ groupMessages: {}, loadedProjects: {} })
  const older = useStore.getState().loadGroupMessages('p1')
  const newer = useStore.getState().loadGroupMessages('p1')
  assert.equal(waiting.length, 2)
  waiting[1](page('new transcript'))
  await newer
  waiting[0](page('old fallback'))
  await older
  assert.equal(useStore.getState().groupMessages.p1?.[0]?.content, 'new transcript')
})
