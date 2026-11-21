import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'

interface BotRow {
  id: string
  parent_bot_id: string | null
  [k: string]: unknown
}
const bots = async (m: MockTransport) =>
  ((await m.request('GET', '/state')) as { projects: { bots: BotRow[] }[] }).projects.flatMap((p) => p.bots)

test('bot 物件沒有 in_flight_turn（daemon 的 /api/state 不帶回合）', async () => {
  const m = new MockTransport()
  for (const b of await bots(m)) assert.ok(!('in_flight_turn' in b), `${b.id} 不該有 in_flight_turn`)
})

test('bot 物件帶 herdr_session（字串）', async () => {
  const m = new MockTransport()
  for (const b of await bots(m)) assert.equal(typeof b.herdr_session, 'string', b.id)
})

test('預覽：沒開是 null，開了帶 {status, port}，關掉又回 null', async () => {
  const m = new MockTransport()
  const top = (await bots(m)).find((b) => !b.parent_bot_id)!
  const of = async () => (await bots(m)).find((b) => b.id === top.id)!.preview
  assert.equal(await of(), null)
  await m.request('POST', `/bots/${top.id}/preview`, { mode: 'spawn' })
  const p = (await of()) as { status: string; port: number | null }
  assert.ok(p && typeof p === 'object', '開預覽之後 state 要帶 preview')
  assert.ok(['starting', 'running'].includes(p.status))
  assert.deepEqual(Object.keys(p).sort(), ['port', 'status'])
  await m.request('DELETE', `/bots/${top.id}/preview`)
  assert.equal(await of(), null)
})
