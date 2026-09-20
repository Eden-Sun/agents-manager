import test from 'node:test'
import assert from 'node:assert/strict'
import { chipStateText } from './chipStateText.ts'

// 晶片的狀態只靠紅點／黃點／數字：按鈕的可及名稱是「名字 3」，讀屏聽不出是未讀、還是要回答。
test('需要回答排第一，且不重複念未讀數', () => {
  assert.equal(chipStateText({ needsReply: true, unread: 2, waitsKids: false, working: false }), '（需要回應）')
})
test('未讀、等子 agent、執行中各自有字', () => {
  assert.equal(chipStateText({ needsReply: false, unread: 3, waitsKids: false, working: false }), '（3 個回合未讀）')
  assert.equal(chipStateText({ needsReply: false, unread: 0, waitsKids: true, working: false }), '（等子 agent）')
  assert.equal(chipStateText({ needsReply: false, unread: 0, waitsKids: false, working: true }), '（執行中）')
})
test('沒有狀態就沒有字', () => {
  assert.equal(chipStateText({ needsReply: false, unread: 0, waitsKids: false, working: false }), '')
})
