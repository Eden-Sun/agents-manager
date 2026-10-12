import test from 'node:test'
import assert from 'node:assert/strict'
import { applyRecipients, hasAllMention } from './groupRecipients.ts'

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
