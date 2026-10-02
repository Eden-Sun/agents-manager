import test from 'node:test'
import assert from 'node:assert/strict'
import { askAnswersText, parseAskAnswers } from './askAnswers.ts'

const content = (over: Record<string, unknown> = {}) =>
  JSON.stringify({
    type: 'ask_answers',
    tool_use_id: 't1',
    answered: true,
    items: [
      { header: '拋單倉庫', question: '拋單怎麼處理？', answer: '拿掉拋單' },
      { question: 'go API 放哪？', answer: null, notes: '再想想' },
    ],
    ...over,
  })
const msg = (c: string, over: Record<string, unknown> = {}) => ({ id: 'ask:C1:t1', role: 'system' as const, content: c, ...over })

test('daemon 記下的問答認得出來：每題的題目、答案、沒答的是 null', () => {
  const a = parseAskAnswers(msg(content()))
  assert.ok(a)
  assert.equal(a.toolUseId, 't1')
  assert.equal(a.answered, true)
  assert.deepEqual(a.items[0], { header: '拋單倉庫', question: '拋單怎麼處理？', answer: '拿掉拋單', notes: null })
  assert.deepEqual(a.items[1], { header: null, question: 'go API 放哪？', answer: null, notes: '再想想' })
})

test('全部沒答（Cancel／Esc）：answered 是 false，複製的文字寫「沒有回答」', () => {
  const a = parseAskAnswers(msg(content({ answered: false, items: [{ question: 'q', answer: null }] })))
  assert.ok(a)
  assert.equal(a.answered, false)
  assert.match(askAnswersText(a), /→ 沒有回答/)
})

test('自訂文字照原文，多行不動', () => {
  const a = parseAskAnswers(msg(content({ items: [{ question: 'q', answer: '自己寫的\n第二行 <b>不是標籤</b>' }] })))
  assert.equal(a?.items[0].answer, '自己寫的\n第二行 <b>不是標籤</b>')
})

test('不是這種訊息就是 null：使用者打的字、別的系統訊息、id 不對、JSON 壞掉、沒有題目', () => {
  assert.equal(parseAskAnswers(msg(content(), { role: 'user' })), null, '使用者自己打的 JSON 不能被畫成問答')
  assert.equal(parseAskAnswers(msg(content(), { role: 'assistant' })), null)
  assert.equal(parseAskAnswers(msg(content(), { id: '01J0ULID' })), null, '沒有 ask: 前綴＝不是 daemon 記的')
  assert.equal(parseAskAnswers(msg('bot 已被停止')), null)
  assert.equal(parseAskAnswers(msg('{壞掉的 json')), null)
  assert.equal(parseAskAnswers(msg(content({ type: 'other' }))), null)
  assert.equal(parseAskAnswers(msg(content({ items: [] }))), null)
  assert.equal(parseAskAnswers(msg(content({ items: [{ answer: 'x' }] }))), null)
})
