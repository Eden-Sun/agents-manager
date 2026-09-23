import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { RewindControl } from './RewindButton.tsx'
import type { Bot, Message } from '../api/types'

const user: Message = {
  id: 'm1', conversation_id: 'c', turn_id: null, bot_id: 'b1', role: 'user', content: 'hi', source: 'web', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-09-23T00:00:00Z',
}
const bot = (kind: string) => ({ ...({} as Bot), id: 'b1', name: 'rw', kind, managed_by: 'user', herdr_session: null }) as Bot
const render = (m: Message, b: Bot | null, blocked: string | null = null) =>
  renderToStaticMarkup(<RewindControl msg={m} bot={b} blocked={blocked} after={2} />)

test('閒著的 claude：使用者訊息上有「倒回這裡」，可以按', () => {
  const html = render(user, bot('claude'))
  assert.match(html, /倒回這裡/)
  assert.doesNotMatch(html, /disabled/)
})

test('正在跑：按鈕在但不能按，tooltip 講原因', () => {
  const html = render(user, bot('claude'), '它正在跑這一回合，等它結束再倒回')
  assert.match(html, /disabled/)
  assert.match(html, /正在跑/)
})

test('codex／grok、回覆、群組發言：不畫', () => {
  assert.equal(render(user, bot('codex')), '')
  assert.equal(render(user, bot('grok')), '')
  assert.equal(render({ ...user, role: 'assistant' }, bot('claude')), '')
  assert.equal(render({ ...user, group_id: 'g1' }, bot('claude')), '')
})
