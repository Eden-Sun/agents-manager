import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { RewindBarView, RewindPicker } from './RewindBar.tsx'
import type { Bot, Message, Run } from '../api/types'

const m = (id: string, role: Message['role'], content: string, over: Partial<Message> = {}): Message => ({
  id, conversation_id: 'c', turn_id: null, bot_id: 'b1', role, content, source: 'web', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-09-24T01:00:00Z', ...over,
})
const bot = (kind: string) => ({ ...({} as Bot), id: 'b1', name: 'rw', kind, herdr_session: null }) as Bot
const run = (agent_status: string) => ({ ...({} as Run), id: 'r1', bot_id: 'b1', state: 'running', agent_status }) as Run
const msgs = [m('u1', 'user', '第一句'), m('a1', 'assistant', 'ok'), m('u2', 'user', '問錯的那一句'), m('a2', 'assistant', 'ok')]
const render = (b: Bot, r: Run | null, list = msgs) => renderToStaticMarkup(<RewindBarView botId="b1" bot={b} run={r} messages={list} />)

test('閒著的 claude、有訊息：「⟲ 倒回」可以按', () => {
  const html = render(bot('claude'), run('idle'))
  assert.match(html, /⟲ 倒回/)
  assert.doesNotMatch(html, /disabled/)
})

test('不能倒：disabled＋title 講原因（codex、正在跑、沒有可倒的）', () => {
  for (const [html, why] of [
    [render(bot('codex'), run('idle')), /只有 claude/],
    [render(bot('claude'), run('working')), /正在跑/],
    [render(bot('claude'), run('idle'), [m('a', 'assistant', 'hi')]), /沒有可以倒回/],
  ] as const) {
    assert.match(html, /disabled/)
    assert.match(html, why)
  }
})

test('清單畫面：新到舊，選中的是最新一則', () => {
  const items = [m('u3', 'user', '最新'), m('u1', 'user', '較舊')]
  const html = renderToStaticMarkup(<RewindPicker items={items} selected="u3" onSelect={() => {}} />)
  assert.ok(html.indexOf('最新') < html.indexOf('較舊'), '新的在上面')
  assert.equal((html.match(/checked=""/g) ?? []).length, 1)
  assert.match(html, /rewind-pick-item on"><input type="radio" name="rewind-pick" checked=""\/><span class="rewind-pick-text">最新/)
})
