import test from 'node:test'
import assert from 'node:assert/strict'
import { queuedSendFor } from './queuedSend.ts'
import type { Message, Turn } from '../api/types.ts'

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
    client_request_id: 'cr1',
    created_at: '2026-09-30T00:00:00Z',
    completed_at: null,
    ...extra,
  }) as Turn

const message = (extra: Partial<Message> = {}) =>
  ({
    id: 'm1',
    conversation_id: 'c1',
    turn_id: 't1',
    bot_id: 'b1',
    role: 'user',
    content: '排隊那句',
    source: 'web',
    incomplete: false,
    group_id: null,
    attachments: [{ id: 'a1', name: 'image.png', mime: 'image/png', size: 10, path: '/a1' }],
    relay_from: null,
    terminal_snapshot: null,
    created_at: '2026-09-30T00:00:00Z',
    ...extra,
  }) as Message

test('queuedSendFor projects only an awaits_idle queued turn and its user message', () => {
  const state = {
    turns: { b1: { t1: turn(), t2: turn({ id: 't2', awaitsIdle: false, awaitsStart: true }) } },
    messages: { b1: [message()] },
  }
  assert.deepEqual(queuedSendFor(state, 'b1'), { turnId: 't1', text: '排隊那句', attachments: ['a1'] })
  assert.equal(queuedSendFor(state, 'b2'), null)
  assert.equal(queuedSendFor({ ...state, turns: { b1: { t1: turn({ status: 'in_flight' }) } } }, 'b1'), null)
})
