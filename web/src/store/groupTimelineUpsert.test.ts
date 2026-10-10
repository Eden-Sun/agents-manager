import test from 'node:test'
import assert from 'node:assert/strict'
import { reset } from './storeEnv.harness.ts'
import { dispatchFrameForTest, useStore } from './store.ts'
import type { Bot, Message, Project } from '../api/types.ts'

// #1050：daemon 會用同一個 id 再推一次 message_added（原文蓋掉終端擷取、補完的回音、補標的來源）。
// bot 對話會換成新內容；群組時間軸也要換，不能停在被截斷的那版。

const project = { id: 'p1', label: 'p', path: '/p', host: 'local' } as Project
const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', identity: null } as Bot
const clipped: Message = {
  id: 'm1',
  conversation_id: 'c1',
  turn_id: 't1',
  role: 'assistant',
  content: 'clipped',
  source: 'terminal_fallback',
  incomplete: true,
  group_id: null,
  attachments: [],
  relay_from: null,
  terminal_snapshot: null,
  created_at: '2026-10-03T00:00:00.000Z',
  seq: 5,
} as Message

const hookFrame = (over: Partial<Message> = {}) => ({
  type: 'message_added',
  data: { bot_id: 'b1', message: { ...clipped, content: 'full text', source: 'hook', incomplete: false, ...over } },
})

test('同 id 的 message_added 會換掉群組時間軸裡那一則', () => {
  reset()
  useStore.setState({
    projects: [project],
    bots: [bot],
    loadedProjects: { p1: true },
    loadedBots: { b1: true },
    messages: { b1: [clipped] },
    groupMessages: { p1: [{ ...clipped, bot_id: 'b1', bot_name: 'b1' }] },
  })

  dispatchFrameForTest(hookFrame())

  const group = useStore.getState().groupMessages.p1
  assert.equal(group.length, 1)
  assert.equal(group[0].content, 'full text')
  assert.equal(group[0].source, 'hook')
  assert.equal(group[0].incomplete, false)
  assert.equal(group[0].bot_name, 'b1')
  assert.equal(useStore.getState().messages.b1[0].content, 'full text', 'bot 對話既有行為不回歸')
})

test('內容一樣的重送不換陣列', () => {
  reset()
  useStore.setState({
    projects: [project],
    bots: [bot],
    loadedProjects: { p1: true },
    loadedBots: { b1: true },
    messages: { b1: [clipped] },
    groupMessages: { p1: [{ ...clipped, bot_id: 'b1', bot_name: 'b1' }] },
  })

  dispatchFrameForTest(hookFrame())
  const first = useStore.getState().groupMessages.p1
  dispatchFrameForTest(hookFrame())

  assert.equal(useStore.getState().groupMessages.p1, first, '同一版內容＝同一個參考，不重畫')
})

test('新 id 照舊插入群組時間軸', () => {
  reset()
  useStore.setState({
    projects: [project],
    bots: [bot],
    loadedProjects: { p1: true },
    loadedBots: { b1: true },
    messages: { b1: [clipped] },
    groupMessages: { p1: [{ ...clipped, bot_id: 'b1', bot_name: 'b1' }] },
  })

  dispatchFrameForTest({
    type: 'message_added',
    data: { bot_id: 'b1', message: { ...clipped, id: 'm2', seq: 6, content: 'second', source: 'hook', incomplete: false } },
  })

  assert.deepEqual(useStore.getState().groupMessages.p1.map((m) => m.id), ['m1', 'm2'])
})
