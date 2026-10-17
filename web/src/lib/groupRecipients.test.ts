import test from 'node:test'
import assert from 'node:assert/strict'
import { parseMentions } from '../api/mentions.ts'
import { applyRecipients, hasAllMention, nextMentionSnapshot, resolveTargetsById } from './groupRecipients.ts'

const names = ['bot-a', 'my bot']

test('切換收件者 chip 只改前綴：內文的連續空白與換行原樣保留', () => {
  assert.equal(applyRecipients('a    b', [], names), 'a    b')
  assert.equal(applyRecipients('print(a    b)\n    return', ['bot-a'], names), '@bot-a print(a    b)\n    return')
})

test('內文裡不是群組成員的 @字樣（例如請看 @alice）不能被靜靜刪掉', () => {
  assert.equal(applyRecipients('請看 @alice 的 PR', ['bot-a'], names), '@bot-a 請看 @alice 的 PR')
})

test('成員的 @ 提及會被換成前綴，不會重複留在內文', () => {
  assert.equal(applyRecipients('@bot-a 你好', ['my bot'], names), '@my bot 你好')
})

test('全部收件者（@all）的偵測與 @all 前綴不變', () => {
  assert.equal(hasAllMention('@all 大家好'), true)
  assert.equal(applyRecipients('大家好', 'all', names), '@all 大家好')
})

test('收件人在打字當下用 id 定下來：之後改名或舊名被別人接手都不會漏送、送錯（#1796）', () => {
  const before = [
    { id: 'b1', name: 'bot-a' },
    { id: 'b2', name: 'bot-b' },
  ]
  const text = '@bot-a 幫我看'
  const snap = nextMentionSnapshot(null, text, before)
  // 送出前 b1 改名、舊名 bot-a 被新來的 b3 接手。
  const after = [
    { id: 'b1', name: 'bot-renamed' },
    { id: 'b2', name: 'bot-b' },
    { id: 'b3', name: 'bot-a' },
  ]
  const kept = nextMentionSnapshot(snap, text, after)
  assert.equal(kept, snap, '文字沒變就沿用打字當下的名單')
  const ids = resolveTargetsById(parseMentions(text, kept.members), after).map((m) => [m.id, m.name])
  assert.deepEqual(ids, [['b1', 'bot-renamed']], '收件人仍是 b1（顯示現在的名字），不是接手舊名的 b3')
  const rewritten = nextMentionSnapshot(snap, `${text}！`, after)
  assert.notEqual(rewritten, snap, '文字一變就用當下的名單重新對一次')
})

test('打字當下的成員之後被刪掉：不再算收件人', () => {
  const before = [{ id: 'b1', name: 'bot-a' }]
  const snap = nextMentionSnapshot(null, '@all 大家好', before)
  assert.deepEqual(resolveTargetsById(parseMentions('@all 大家好', snap.members), []), [])
})
