/**
 * 群組送出的已消耗鍵（#1815）：回應遺失之後，同一句話只有在「每一個收件人的回合都已是終態」時才是新的動作；
 * 還有人在進行中或本機沒看到，就沿用原 crid（daemon 冪等，不再打字）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project, Turn } from '../api/types.ts'

const { useStore } = await import('./store.ts')

const bot = (id: string) => ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null }) as Bot
const json = (body: unknown, status: number) => new Response(JSON.stringify(body), { status })
const turn = (id: string, status: Turn['status'], crid: string) =>
  ({ id, conversation_id: '', run_id: null, bot_id: id, origin: 'web', status, client_request_id: crid, created_at: '', completed_at: null, delivery: 'ok' }) as Turn

function seed() {
  reset()
  useStore.setState({
    projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local' } as Project],
    bots: [bot('b1'), bot('b2')],
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    drafts: {},
    notices: [],
    turns: {},
    runs: {},
  })
}

/** 第一次送出回應遺失，回傳第一次用的 crid。 */
async function lostFirstSend(recipientIds: string[]): Promise<string> {
  seed()
  let n = 0
  routeDaemon(() => {
    n += 1
    if (n === 1) throw new TypeError('Failed to fetch')
    return json({ group_id: 'g1', sent: [], skipped: [] }, 200)
  })
  assert.equal(await useStore.getState().sendGroupChat('p1', '@all 跑測試', [], recipientIds), null)
  return (requests.filter((r) => r.path.endsWith('/chat')).at(-1)!.body as { client_request_id: string }).client_request_id
}

async function nextCrid(recipientIds: string[]): Promise<string> {
  await useStore.getState().sendGroupChat('p1', '@all 跑測試', [], recipientIds)
  return (requests.filter((r) => r.path.endsWith('/chat')).at(-1)!.body as { client_request_id: string }).client_request_id
}

test('回應遺失、所有收件人都已 completed：同一句是新的動作，換新的 crid', async () => {
  const crid = await lostFirstSend(['b1', 'b2'])
  useStore.setState({
    turns: {
      b1: { t1: turn('t1', 'completed', `${crid}:b1`) },
      b2: { t2: turn('t2', 'completed', `${crid}:b2`) },
    },
  })
  assert.notEqual(await nextCrid(['b1', 'b2']), crid)
})

test('回應遺失、部分收件人還在 in_flight：沿用原 crid（daemon 冪等）', async () => {
  const crid = await lostFirstSend(['b1', 'b2'])
  useStore.setState({
    turns: {
      b1: { t1: turn('t1', 'completed', `${crid}:b1`) },
      b2: { t2: turn('t2', 'in_flight', `${crid}:b2`) },
    },
  })
  assert.equal(await nextCrid(['b1', 'b2']), crid)
})

test('回應遺失、有收件人本機沒看到回合（未知）：沿用原 crid', async () => {
  const crid = await lostFirstSend(['b1', 'b2'])
  useStore.setState({ turns: { b1: { t1: turn('t1', 'completed', `${crid}:b1`) } } })
  assert.equal(await nextCrid(['b1', 'b2']), crid)
})

test('回應遺失、本機一個回合都沒有（剛失敗）：沿用原 crid', async () => {
  const crid = await lostFirstSend(['b1', 'b2'])
  assert.equal(await nextCrid(['b1', 'b2']), crid)
})
