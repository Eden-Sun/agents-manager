import test from 'node:test'
import assert from 'node:assert/strict'
import { parseMentions } from './mentions.ts'

/** 前端的收件者預覽要跟 daemon（`group.rs::parse_mentions`，#657）選到同一批 bot，不然 chip 寫「→ @Claude」、訊息實際送給 `claude`。 */
const m = (...names: string[]) => names.map((name, i) => ({ id: String(i + 1), name }))
const pick = (text: string, members: { id: string; name: string }[]) => parseMentions(text, members).map((x) => x.name)

test('名字只差大小寫的兩顆：各打各的，摺過的寫法兩顆都不算', () => {
  const both = m('Claude', 'claude')
  assert.deepEqual(pick('@claude look', both), ['claude'])
  assert.deepEqual(pick('@Claude look', both), ['Claude'])
  assert.deepEqual(pick('@CLAUDE look', both), [], '摺完有兩顆＝不明確，誰都不算')
})

test('只剩一顆時仍不分大小寫', () => {
  assert.deepEqual(pick('@CLAUDE look', m('Claude')), ['Claude'])
  assert.deepEqual(pick('@claude, look', m('Claude', 'codex')), ['Claude'])
})

test('名字有空白：完全相同的優先，沒有時摺完剛好一顆才算', () => {
  const both = m('My Bot', 'my bot')
  assert.deepEqual(pick('@my bot do', both), ['my bot'])
  assert.deepEqual(pick('@My Bot do', both), ['My Bot'])
  assert.deepEqual(pick('@MY BOT do', both), [])
  assert.deepEqual(pick('@MY BOT do', m('My Bot')), ['My Bot'])
})

test('@all 不分大小寫、其餘照舊', () => {
  assert.deepEqual(pick('hi @ALL', m('a', 'b')), ['a', 'b'])
  assert.deepEqual(pick('me@example.com', m('example')), [])
  assert.deepEqual(pick('@a- look', m('a', 'b')), ['a'])
})

test('非 ASCII 的大小寫摺疊 daemon 不做（只摺 ASCII）：@élan 不等於 Élan', () => {
  assert.deepEqual(pick('@élan', m('Élan')), [])
  assert.deepEqual(pick('@Élan', m('Élan')), ['Élan'])
})
