import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import { dispatchFrameForTest, useStore } from './store.ts'
import type { Bot, GroupMessage, Project } from '../api/types.ts'

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })
const project = { id: 'p1', label: 'p', path: '/p', host: 'local' } as Project
const bot = (name: string, id = 'b1') => ({ id, name, project_id: 'p1', kind: 'claude', identity: null }) as Bot
const message = (botName: string, botId = 'b1'): GroupMessage => ({
  id: `m-${botId}-${botName}`,
  conversation_id: 'c1',
  turn_id: 't1',
  bot_id: botId,
  bot_name: botName,
  role: 'assistant',
  content: 'reply',
  source: 'hook',
  incomplete: false,
  group_id: null,
  attachments: [],
  relay_from: null,
  terminal_snapshot: null,
  created_at: '2026-10-03T00:00:00.000Z',
  seq: 1,
})

async function settle(predicate: () => boolean) {
  for (let i = 0; i < 100; i++) {
    if (predicate()) return
    await new Promise<void>((resolve) => setTimeout(resolve, 5))
  }
}

test('bot 改名後更新已載入群組訊息的徽章名稱，保留已翻出的歷史頁', async () => {
  const history = Array.from({ length: 250 }, (_, i) => ({ ...message('before'), id: `m-${i}`, seq: i + 1 }))
  reset()
  useStore.setState({
    projects: [project],
    bots: [bot('before')],
    loadedProjects: { p1: true },
    selectedProjectId: 'p1',
    groupMessages: { p1: history },
  })
  routeDaemon((r) => {
    if (r.path === '/api/state') {
      return json({ daemon_seq: 1, projects: [{ ...project, bots: [bot('after')] }], bots: [bot('after')], runs: [], turns: [] })
    }
    return json({})
  })

  dispatchFrameForTest({ type: 'bot_changed', data: { bot_id: 'b1' } })
  await settle(() => useStore.getState().groupMessages.p1?.every((m) => m.bot_name === 'after'))

  assert.equal(requests.some((r) => r.path.startsWith('/api/projects/p1/messages')), false, '只需把快取徽章同步到新 bot 名稱')
  assert.equal(useStore.getState().groupMessages.p1.length, 250, '重命名不能丟掉已翻出的歷史頁')
})

test('bot 刪除後從已載入群組訊息移除該 bot，保留同專案其他 bot 歷史', async () => {
  reset()
  useStore.setState({
    projects: [project],
    bots: [bot('deleted'), bot('other', 'b2')],
    loadedProjects: { p1: true },
    selectedProjectId: 'p1',
    groupMessages: { p1: [message('deleted'), message('other', 'b2')] },
  })
  routeDaemon((r) => {
    if (r.path === '/api/state') {
      return json({ daemon_seq: 1, projects: [{ ...project, bots: [bot('other', 'b2')] }], bots: [bot('other', 'b2')], runs: [], turns: [] })
    }
    return json({})
  })

  dispatchFrameForTest({ type: 'bot_changed', data: { bot_id: 'b1' } })
  await settle(() => useStore.getState().groupMessages.p1?.every((m) => m.bot_id !== 'b1'))

  assert.equal(requests.some((r) => r.path.startsWith('/api/projects/p1/messages')), false)
  assert.deepEqual(useStore.getState().groupMessages.p1.map((m) => m.bot_id), ['b2'])
})

test('已刪 bot 復原後，合併最近頁但保留本機已翻出的群組歷史', async () => {
  reset()
  useStore.setState({
    projects: [project],
    bots: [bot('other', 'b2')],
    loadedProjects: { p1: true },
    selectedProjectId: 'p1',
    groupMessages: { p1: [{ ...message('other', 'b2'), id: 'old-other', seq: 1 }] },
  })
  routeDaemon((r) => {
    if (r.path === '/api/state') {
      return json({ daemon_seq: 1, projects: [{ ...project, bots: [bot('restored'), bot('other', 'b2')] }], bots: [bot('restored'), bot('other', 'b2')], runs: [], turns: [] })
    }
    if (r.path.startsWith('/api/projects/p1/messages')) {
      return json({ project_id: 'p1', messages: [{ ...message('restored'), seq: 2 }], has_more: true })
    }
    return json({})
  })

  dispatchFrameForTest({ type: 'bot_changed', data: { bot_id: 'b1' } })
  await settle(() => useStore.getState().groupMessages.p1?.some((m) => m.bot_id === 'b1'))

  assert.ok(requests.some((r) => r.path.startsWith('/api/projects/p1/messages')))
  assert.deepEqual(useStore.getState().groupMessages.p1.map((m) => m.id), ['old-other', 'm-b1-restored'])
})

test('復原補頁在飛時收到同一訊息的新版本，舊頁回來不能蓋掉新版本', async () => {
  reset()
  useStore.setState({
    projects: [project],
    bots: [bot('other', 'b2')],
    loadedProjects: { p1: true },
    selectedProjectId: 'p1',
    groupMessages: { p1: [{ ...message('other', 'b2'), id: 'old-other', seq: 1 }] },
  })
  let resolvePage!: (response: Response) => void
  const pendingPage = new Promise<Response>((resolve) => {
    resolvePage = resolve
  })
  routeDaemon((r) => {
    if (r.path === '/api/state') {
      return json({ daemon_seq: 1, projects: [{ ...project, bots: [bot('restored'), bot('other', 'b2')] }], bots: [bot('restored'), bot('other', 'b2')], runs: [], turns: [] })
    }
    if (r.path.startsWith('/api/projects/p1/messages')) return pendingPage
    return json({})
  })

  dispatchFrameForTest({ type: 'bot_changed', data: { bot_id: 'b1' } })
  await settle(() => requests.some((r) => r.path.startsWith('/api/projects/p1/messages')))
  dispatchFrameForTest({
    type: 'message_added',
    data: { bot_id: 'b1', message: { ...message('restored'), content: 'fresh websocket version', seq: 2 } },
  })
  resolvePage(json({ project_id: 'p1', messages: [{ ...message('restored'), content: 'stale page version', seq: 2 }], has_more: true }))
  await settle(() => useStore.getState().moreMessages.p1 === true)

  assert.equal(useStore.getState().groupMessages.p1.find((m) => m.id === 'm-b1-restored')?.content, 'fresh websocket version')
})

test('bot 復原補頁在飛時又被刪除，舊頁回來不能把訊息加回來', async () => {
  reset()
  useStore.setState({
    projects: [project],
    bots: [bot('other', 'b2')],
    loadedProjects: { p1: true },
    selectedProjectId: 'p1',
    groupMessages: { p1: [{ ...message('other', 'b2'), id: 'old-other', seq: 1 }] },
  })
  let stateRequests = 0
  let resolvePage!: (response: Response) => void
  const pendingPage = new Promise<Response>((resolve) => {
    resolvePage = resolve
  })
  routeDaemon((r) => {
    if (r.path === '/api/state') {
      stateRequests++
      const bots = stateRequests === 1 ? [bot('restored'), bot('other', 'b2')] : [bot('other', 'b2')]
      return json({ daemon_seq: stateRequests, projects: [{ ...project, bots }], bots, runs: [], turns: [] })
    }
    if (r.path.startsWith('/api/projects/p1/messages')) return pendingPage
    return json({})
  })

  dispatchFrameForTest({ type: 'bot_changed', data: { bot_id: 'b1' } })
  await settle(() => requests.some((r) => r.path.startsWith('/api/projects/p1/messages')))
  dispatchFrameForTest({ type: 'bot_changed', data: { bot_id: 'b1' } })
  await settle(() => stateRequests === 2 && useStore.getState().bots.every((b) => b.id !== 'b1'))
  resolvePage(json({ project_id: 'p1', messages: [{ ...message('restored'), seq: 2 }], has_more: true }))
  await settle(() => useStore.getState().moreMessages.p1 === true)

  assert.equal(useStore.getState().groupMessages.p1.some((m) => m.bot_id === 'b1'), false)
})
