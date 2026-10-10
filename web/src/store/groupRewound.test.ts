import test from 'node:test'
import assert from 'node:assert/strict'
import { reset } from './storeEnv.harness.ts'
import { dispatchFrameForTest, useStore } from './store.ts'
import type { Bot, GroupMessage, Message, Project } from '../api/types.ts'

// #1051：倒回（messages_rewound）只改 bot 對話清單時，群組時間軸同一批訊息不會變「已倒回」。

const project = { id: 'p1', label: 'p', path: '/p', host: 'local' } as Project
const bot = (id: string) => ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null }) as Bot
const user = (id: string, botId: string, conv: string, seq: number): Message =>
  ({
    id,
    conversation_id: conv,
    turn_id: null,
    bot_id: botId,
    role: 'user',
    content: id,
    source: 'web',
    incomplete: false,
    group_id: null,
    attachments: [],
    relay_from: null,
    terminal_snapshot: null,
    created_at: '2026-10-03T00:00:00.000Z',
    seq,
  }) as Message
const grp = (m: Message): GroupMessage => ({ ...m, bot_name: m.bot_id }) as GroupMessage

test('倒回同時標群組時間軸裡同一段對話的訊息，別顆 bot 的不動', () => {
  reset()
  const u1 = user('u1', 'b1', 'cb1', 1)
  const x = user('x', 'b2', 'cb2', 2)
  const u2 = user('u2', 'b1', 'cb1', 3)
  useStore.setState({
    projects: [project],
    bots: [bot('b1'), bot('b2')],
    loadedProjects: { p1: true },
    messages: { b1: [u1, u2] },
    groupMessages: { p1: [grp(u1), grp(x), grp(u2)] },
  })

  dispatchFrameForTest({ type: 'messages_rewound', data: { bot_id: 'b1', message_id: 'u1', rewound_at: 'T' } })

  const state = useStore.getState()
  assert.deepEqual(state.groupMessages.p1.map((m) => m.rewound_at ?? null), ['T', null, 'T'])
  assert.deepEqual(state.messages.b1.map((m) => m.rewound_at), ['T', 'T'])
})
