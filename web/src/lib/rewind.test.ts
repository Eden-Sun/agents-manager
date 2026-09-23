import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError, type Message } from '../api/types'
import { canOfferRewind, markRewound, rewindBlocked, rewindErrText } from './rewind'

const msg = (id: string, role: Message['role'] = 'user', over: Partial<Message> = {}): Message => ({
  id, conversation_id: 'c', turn_id: null, bot_id: 'b', role, content: id, source: 'web', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-09-23T00:00:00Z', ...over,
})
const claude = { kind: 'claude' as const, herdr_session: null }

test('claude（含子 agent）的使用者訊息給倒回；codex／grok、default session、回覆、已倒掉的都不給', () => {
  assert.equal(canOfferRewind(msg('u'), claude), true)
  assert.equal(canOfferRewind(msg('a', 'assistant'), claude), false)
  assert.equal(canOfferRewind(msg('u', 'user', { rewound_at: '2026-09-23T01:00:00Z' }), claude), false)
  assert.equal(canOfferRewind(msg('u'), { ...claude, kind: 'codex' }), false)
  assert.equal(canOfferRewind(msg('u'), { ...claude, kind: 'grok' }), false)
  assert.equal(canOfferRewind(msg('u'), { ...claude, herdr_session: 'default' }), false)
  assert.equal(canOfferRewind(msg('u'), null), false)
})

test('只有閒著才能按：跑回合、卡提問、沒在跑都講得出為什麼', () => {
  assert.equal(rewindBlocked({ state: 'running', agent_status: 'idle' }), null)
  assert.match(rewindBlocked({ state: 'running', agent_status: 'working' }) ?? '', /正在跑/)
  assert.match(rewindBlocked({ state: 'running', agent_status: 'blocked' }) ?? '', /提問/)
  assert.match(rewindBlocked({ state: 'stopped', agent_status: 'idle' }) ?? '', /沒在跑/)
  assert.match(rewindBlocked(null) ?? '', /沒在跑/)
})

test('那則與之後的標成倒回，之前的不動；找不到那則不改清單', () => {
  const list = [msg('u1'), msg('a1', 'assistant'), msg('u2'), msg('a2', 'assistant')]
  const next = markRewound(list, 'u2', 'T')
  assert.ok(next)
  assert.deepEqual(next.map((m) => m.rewound_at ?? null), [null, null, 'T', 'T'])
  assert.equal(markRewound(list, 'nope', 'T'), null)
  // 已經全標過：不產生新清單（避免無謂 re-render），也不覆蓋第一次的時間。
  assert.equal(markRewound(next, 'u2', 'T2'), null)
})

test('失敗訊息：daemon 寫好的 message 直接用；舊 daemon（405／路由 404）講要重建', () => {
  assert.equal(rewindErrText(new ApiError(409, { reason: 'not_idle', message: '它正在忙，等這一回合結束再倒回。' }, 'x')), '它正在忙，等這一回合結束再倒回。')
  assert.match(rewindErrText(new ApiError(405, {}, 'x')), /重建並重啟 daemon/)
  assert.match(rewindErrText(new ApiError(404, {}, 'x')), /重建並重啟 daemon/)
  assert.equal(rewindErrText(new ApiError(404, { error: 'not_found', what: 'message' }, 'x')), '找不到這則訊息。')
})
