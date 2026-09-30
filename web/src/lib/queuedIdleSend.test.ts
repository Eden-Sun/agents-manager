import test from 'node:test'
import assert from 'node:assert/strict'
import type { Message, Turn } from '../api/types'
import { queuedIdleSend } from './queuedIdleSend.ts'

const turn = (id: string, patch: Record<string, unknown> = {}) =>
  ({ id, status: 'queued', awaitsIdle: true, created_at: id, ...patch }) as unknown as Turn

const message = (id: string, turnId: string, content: string, attachments: string[] = []) =>
  ({
    id,
    turn_id: turnId,
    role: 'user',
    content,
    attachments: attachments.map((attachmentId) => ({ id: attachmentId })),
  }) as unknown as Message

test('projects the daemon queued-idle turn and its exact message attachments', () => {
  const selected = queuedIdleSend(
    {
      awaiting: turn('turn-awaiting'),
      later: turn('turn-later', { created_at: 'z' }),
      starting: turn('turn-starting', { awaitsIdle: false, awaitsStart: true }),
      assignment: turn('turn-assignment', { awaitsIdle: false, origin: 'external' }),
    },
    [
      message('other', 'other-turn', '忽略'),
      message('queued-message', 'turn-awaiting', '請接著幫我查', ['att-1', 'att-2']),
    ],
  )

  assert.ok(selected)
  assert.equal(selected.turnId, 'turn-awaiting')
  assert.equal(selected.text, '請接著幫我查')
  assert.deepEqual(selected.attachments, ['att-1', 'att-2'])
})

test('ignores queued turns without awaits_idle and returns null after the queued turn leaves the queue', () => {
  assert.equal(
    queuedIdleSend({ t: turn('t', { awaitsIdle: false, awaitsStart: true }) }, [message('m', 't', '等啟動')]),
    null,
  )
  assert.equal(
    queuedIdleSend({ t: turn('t', { status: 'in_flight' }) }, [message('m', 't', '已領走')]),
    null,
  )
})
