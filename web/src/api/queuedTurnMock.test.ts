import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'

test('mock prompt persists one awaits_idle turn, rejects a second slot, and withdraws with text and attachment IDs', async () => {
  const mock = new MockTransport()
  // Seed an active, non-idle run directly. This also covers the API condition where agent_status
  // is busy even when there is no in_flight turn row yet.
  const internals = mock as unknown as { bots: { id: string; name: string }[]; runs: unknown[] }
  const botId = internals.bots.find((bot) => bot.name === 'am-claude')!.id
  internals.runs.push({ id: 'r1', bot_id: botId, state: 'running', agent_status: 'working' })

  const queued = await mock.request('POST', `/bots/${botId}/prompt`, {
    text: 'queued text',
    client_request_id: 'queued',
    attachments: ['a1', 'a2'],
    queue_if_busy: true,
  }) as { turn_id: string; message_id: string; delivery: string }
  assert.equal(queued.delivery, 'queued')
  await assert.rejects(
    () => mock.request('POST', `/bots/${botId}/prompt`, { text: 'second queued', client_request_id: 'second', queue_if_busy: true }),
    (error: unknown) => error instanceof ApiError && error.status === 409 && error.body.reason === 'queue_slot_taken' && error.body.turn_id === queued.turn_id,
  )

  const snapshot = await mock.request('GET', '/state') as { projects: { bots: { id: string; queued_turn?: { id: string; awaits_idle: number } | null }[] }[] }
  const projected = snapshot.projects.flatMap((project) => project.bots).find((bot) => bot.id === botId)?.queued_turn
  assert.equal(projected?.id, queued.turn_id)
  assert.equal(projected?.awaits_idle, 1)

  assert.deepEqual(await mock.request('POST', `/turns/${queued.turn_id}/withdraw`), { text: 'queued text', attachments: ['a1', 'a2'] })
})
