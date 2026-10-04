import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import { useStore } from './store.ts'
import type { Message } from '../api/types.ts'

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })
const message = (id: string, seq: number, created_at: string): Message => ({
  id,
  conversation_id: 'c1',
  turn_id: null,
  bot_id: 'b1',
  role: 'assistant',
  content: id,
  source: 'hook',
  incomplete: false,
  group_id: null,
  attachments: [],
  relay_from: null,
  terminal_snapshot: null,
  created_at,
  seq,
})

test('系統時鐘倒退後，單 bot 歷史仍依 rowid 游標翻頁並維持時間排序', async () => {
  reset()
  const earlierInsert = message('m11', 11, '2026-10-03T10:00:00.000Z')
  const laterInsert = message('m12', 12, '2026-10-03T08:00:00.000Z')
  useStore.setState({ messages: { b1: [laterInsert, earlierInsert] }, moreMessages: { b1: true } })
  routeDaemon(() => json({
    messages: [message('m10', 10, '2026-10-03T09:00:00.000Z')],
    turns: [],
    has_more: true,
  }))

  await useStore.getState().loadEarlierMessages('b1')

  const pageRequest = requests.find((r) => r.path.includes('/bots/b1/messages') && r.path.includes('before='))
  assert.ok(pageRequest)
  assert.equal(new URL(pageRequest.path, 'http://x').searchParams.get('before'), 'm11', '游標必須是最小 rowid，而非最早 timestamp')
  assert.deepEqual(useStore.getState().messages.b1.map((m) => m.id), ['m12', 'm10', 'm11'], '跨頁合併後仍依 created_at 顯示')
})
