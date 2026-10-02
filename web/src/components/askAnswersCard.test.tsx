import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { AskAnswersCard } from './AskAnswersCard.tsx'
import { parseAskAnswers } from '../lib/askAnswers.ts'
import type { Message } from '../api/types'

const message = (items: unknown[], answered = true): Message => ({
  id: 'ask:C1:t1',
  conversation_id: 'C1',
  turn_id: 'T1',
  role: 'system',
  content: JSON.stringify({ type: 'ask_answers', tool_use_id: 't1', answered, items }),
  source: 'system',
  incomplete: false,
  group_id: null,
  attachments: [],
  relay_from: null,
  terminal_snapshot: null,
  created_at: '2026-10-02T08:34:54.635Z',
})

const render = (m: Message) => {
  const ask = parseAskAnswers(m)
  assert.ok(ask)
  return renderToStaticMarkup(<AskAnswersCard msg={m} ask={ask} />)
}

test('畫成「Claude 問／你答」：每題一列，題目與答案都在，header 當標籤', () => {
  const html = render(message([
    { header: '拋單倉庫', question: '拋單怎麼處理？', answer: '拿掉拋單' },
    { question: 'go API 放哪？', answer: '自訂：gateway' },
  ]))
  assert.equal((html.match(/Claude 問/g) ?? []).length, 2)
  assert.equal((html.match(/你答/g) ?? []).length, 2)
  assert.match(html, /拋單怎麼處理？/)
  assert.match(html, /<span class="ask-header">拋單倉庫<\/span>/)
  assert.match(html, /拿掉拋單/)
  assert.match(html, /自訂：gateway/)
  assert.match(html, /提問與回答/)
})

test('不是使用者打的 prompt：沒有 user 泡泡、倒回鍵、送達標或重送入口', () => {
  const html = render(message([{ question: 'q', answer: 'a' }]))
  assert.doesNotMatch(html, /class="msg user|class="msg system|rewind|倒回|未驗證送達|重送|queued/i)
  assert.match(html, /class="msg ask-answers"/)
})

test('取消：每題寫「沒有回答」，標頭註明', () => {
  const html = render(message([{ question: 'q1', answer: null }, { question: 'q2', answer: null }], false))
  assert.equal((html.match(/沒有回答/g) ?? []).length, 3, '兩題＋標頭')
  assert.match(html, /ask-a none/)
})

test('答案裡的 HTML 不會被當標籤執行', () => {
  const html = render(message([{ question: 'q', answer: '<script>alert(1)</script>' }]))
  assert.doesNotMatch(html, /<script>/)
  assert.match(html, /&lt;script&gt;/)
})

test('對話泡泡：ask 系統訊息走問答卡；使用者自己打一樣長相的 JSON 仍是使用者泡泡', async () => {
  const { Bubble } = await import('./ChatPanel.tsx')
  const m = message([{ question: 'q', answer: 'a' }])
  const card = renderToStaticMarkup(<Bubble msg={m} />)
  assert.match(card, /class="msg ask-answers"/)
  const typed = renderToStaticMarkup(<Bubble msg={{ ...m, id: '01J0ULID', role: 'user', source: 'web' }} />)
  assert.match(typed, /class="msg user/)
  assert.doesNotMatch(typed, /ask-answers/)
})
