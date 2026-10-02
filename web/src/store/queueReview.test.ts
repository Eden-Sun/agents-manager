/**
 * 輸入框與待送出佇列的對抗式審查（#733 之後）：重送去重鍵的壽命、漏掉 WS 幀之後的「排隊中」殘影。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Message, Project, Turn } from '../api/types.ts'

const { useStore } = await import('./store.ts')
const { queuedSendFor } = await import('./queuedSend.ts')
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude' } as Bot
const project = { id: 'p1', path: '/p1', label: 'p1', host: 'local' } as Project
const turn = (extra: Partial<Turn> = {}) =>
  ({
    id: 't1',
    conversation_id: 'c1',
    run_id: 'r1',
    bot_id: 'b1',
    origin: 'web',
    status: 'queued',
    delivery: 'pending',
    unverified: false,
    autoResend: true,
    awaitsStart: false,
    awaitsIdle: true,
    startError: null,
    client_request_id: 'cr-old',
    created_at: '2026-09-30T00:00:00Z',
    completed_at: null,
    ...extra,
  }) as Turn

function seed() {
  reset()
  useStore.setState({
    bots: [bot],
    projects: [project],
    runs: { b1: { id: 'r1', state: 'running', agent_status: 'working' } as never },
    turns: {},
    messages: {},
    drafts: {},
    notices: [],
  })
}

const promptCrids = () => requests.filter((r) => r.path.endsWith('/prompt')).map((r) => (r.body as { client_request_id: string }).client_request_id)

/**
 * 回應遺失（fetch 直接失敗）時 crid 要留著，讓同一句的重試被 daemon 認成同一件事。但它不能永遠留著：
 * daemon 其實收下了（WS 幀已經把那一回合推進來），使用者之後再打同一句話（或撤回後再送），沿用舊 crid 會被
 * daemon 當成重送，回舊回合的結果——新的這一則根本沒送出去，畫面卻以為送了（輸入框清空）。
 */
test('回應遺失後 daemon 其實收下了：同一句話之後再送，不能沿用舊的 crid（否則被當重送、新訊息默默消失）', async () => {
  seed()
  let calls = 0
  routeDaemon((req) => {
    if (!req.path.endsWith('/prompt')) return json({})
    calls++
    if (calls === 1) throw new TypeError('network down after the daemon committed')
    return json({ turn_id: 't2', message_id: 'm2', delivery: 'queued' })
  })
  assert.equal(await useStore.getState().sendPrompt('b1', '幫我跑測試-stale-1', [], false, false, undefined, true), false)
  const [first] = promptCrids()
  // daemon 其實收下了、也已經撤回（或送完）：WS 幀把那一回合推進本機，狀態是終態。
  useStore.setState({ turns: { b1: { t1: turn({ status: 'failed', delivery: 'failed', awaitsIdle: true, client_request_id: first }) } } })

  await useStore.getState().sendPrompt('b1', '幫我跑測試-stale-1', [], false, false, undefined, true)
  const [, second] = promptCrids()
  assert.notEqual(second, first, '舊的 crid 對應的回合已經是終態：這是新的一次送出')
})

test('回應遺失、daemon 的回合還在排隊：重試照舊沿用同一個 crid（那才是真的重送）', async () => {
  seed()
  let calls = 0
  routeDaemon((req) => {
    if (!req.path.endsWith('/prompt')) return json({})
    calls++
    if (calls === 1) throw new TypeError('network down')
    return json({ turn_id: 't1', message_id: 'm1', delivery: 'queued' })
  })
  await useStore.getState().sendPrompt('b1', '幫我跑測試-stale-2', [], false, false, undefined, true)
  const [first] = promptCrids()
  useStore.setState({ turns: { b1: { t1: turn({ client_request_id: first }) } } })
  await useStore.getState().sendPrompt('b1', '幫我跑測試-stale-2', [], false, false, undefined, true)
  assert.deepEqual(promptCrids(), [first, first])
})

test('本機不認得那一回合（別的分頁收下的、重整過）：留太久的 crid 一樣不再沿用', async () => {
  seed()
  const realNow = Date.now
  let calls = 0
  routeDaemon((req) => {
    if (!req.path.endsWith('/prompt')) return json({})
    calls++
    if (calls === 1) throw new TypeError('network down')
    return json({ turn_id: 't2', message_id: 'm2', delivery: 'queued' })
  })
  try {
    assert.equal(await useStore.getState().sendPrompt('b1', '幫我跑測試-stale-3', [], false, false, undefined, true), false)
    const [first] = promptCrids()
    Date.now = () => realNow() + 6 * 60 * 1000
    await useStore.getState().sendPrompt('b1', '幫我跑測試-stale-3', [], false, false, undefined, true)
    assert.notEqual(promptCrids()[1], first)
  } finally {
    Date.now = realNow
  }
})

/**
 * 分頁在背景（手機休眠、斷線）時，那一則排隊的被 flush／撤回，`turn_updated` 幀漏掉了；重連後的 `refreshState`
 * 只會清掉過期的 in_flight，不認得的 queued 會一直留在本機——這個分頁就永遠顯示「排隊中」（#733 的 7788 vs 5173 不一致）。
 */
const stateBody = (queuedTurn: unknown = null) => ({
  daemon_seq: 5,
  projects: [{ ...project, bots: [{ ...bot, queued_turn: queuedTurn }] }],
})

test('漏掉 turn_updated 之後的 refreshState：daemon 已經沒有的 queued 回合不能留成「排隊中」', async () => {
  seed()
  const msg = { id: 'm1', conversation_id: 'c1', turn_id: 't1', bot_id: 'b1', role: 'user', content: '稍後送出', attachments: [] } as unknown as Message
  useStore.setState({ turns: { b1: { t1: turn() } }, messages: { b1: [msg] }, loadedBots: { b1: true }, lastDurableSeq: 3 })
  assert.ok(queuedSendFor(useStore.getState(), 'b1'), '前提：本機以為有一則排隊中')
  routeDaemon((req) => (req.path.endsWith('/state') ? json(stateBody(null)) : json({ messages: [], turns: [], has_more: false })))
  await useStore.getState().refreshState()
  assert.equal(queuedSendFor(useStore.getState(), 'b1'), null, 'daemon 說沒有排隊中的回合')
})

test('refreshState：daemon 還有的 queued 回合照舊留著', async () => {
  seed()
  const msg = { id: 'm1', conversation_id: 'c1', turn_id: 't1', bot_id: 'b1', role: 'user', content: '稍後送出', attachments: [] } as unknown as Message
  useStore.setState({ turns: { b1: { t1: turn() } }, messages: { b1: [msg] }, loadedBots: { b1: true }, lastDurableSeq: 3 })
  routeDaemon((req) =>
    req.path.endsWith('/state')
      ? json(stateBody({ id: 't1', conversation_id: 'c1', run_id: 'r1', bot_id: 'b1', origin: 'web', status: 'queued', delivery: 'pending', awaits_idle: 1, created_at: '2026-09-30T00:00:00Z' }))
      : json({ messages: [], turns: [], has_more: false }),
  )
  await useStore.getState().refreshState()
  assert.equal(queuedSendFor(useStore.getState(), 'b1')?.turnId, 't1')
})

test('refreshState：快照比本機已看到的幀還舊時，不能把剛由 WS 推進來的 queued 回合清掉', async () => {
  seed()
  const msg = { id: 'm1', conversation_id: 'c1', turn_id: 't1', bot_id: 'b1', role: 'user', content: '稍後送出', attachments: [] } as unknown as Message
  // 本機已經看到 seq 9 的幀（那一則剛排進去），這份快照只到 seq 5。
  useStore.setState({ turns: { b1: { t1: turn() } }, messages: { b1: [msg] }, loadedBots: { b1: true }, lastDurableSeq: 9 })
  let fetches = 0
  routeDaemon((req) => {
    if (!req.path.endsWith('/state')) return json({ messages: [], turns: [], has_more: false })
    fetches++
    // 第一份是舊快照（還沒有那一則）；補抓的第二份（seq 9）才有。
    return fetches === 1
      ? json(stateBody(null))
      : json({
          ...stateBody({ id: 't1', conversation_id: 'c1', run_id: 'r1', bot_id: 'b1', origin: 'web', status: 'queued', delivery: 'pending', awaits_idle: 1, created_at: '2026-09-30T00:00:00Z' }),
          daemon_seq: 9,
        })
  })
  await useStore.getState().refreshState()
  assert.equal(queuedSendFor(useStore.getState(), 'b1')?.turnId, 't1', '舊快照不能清掉比它新的回合')
})

test('群組送出同一道理：回應遺失後隔了重送窗口再送同一句，是新的動作', async () => {
  seed()
  const realNow = Date.now
  const crids: string[] = []
  routeDaemon((req) => {
    if (!req.path.endsWith('/chat')) return json({})
    crids.push((req.body as { client_request_id: string }).client_request_id)
    if (crids.length === 1) throw new TypeError('network down')
    return json({ results: [] })
  })
  try {
    await useStore.getState().sendGroupChat('p1', '大家好-stale-4')
    Date.now = () => realNow() + 6 * 60 * 1000
    await useStore.getState().sendGroupChat('p1', '大家好-stale-4')
  } finally {
    Date.now = realNow
  }
  assert.equal(crids.length, 2)
  assert.notEqual(crids[1], crids[0])
})
