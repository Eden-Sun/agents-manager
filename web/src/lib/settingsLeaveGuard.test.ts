import test from 'node:test'
import assert from 'node:assert/strict'
import { registerSettingsLeaveGuard, settingsBlocksLeave } from './settingsLeaveGuard.ts'

test('沒登記：不擋', () => {
  assert.equal(settingsBlocksLeave(), false)
})

test('登記的守門函式原樣透傳 true／false，且每次詢問都真的呼叫它', () => {
  let answer = true
  let calls = 0
  const off = registerSettingsLeaveGuard(() => {
    calls++
    return answer
  })
  assert.equal(settingsBlocksLeave(), true)
  answer = false
  assert.equal(settingsBlocksLeave(), false)
  assert.equal(calls, 2)
  off()
})

test('解除後回 false', () => {
  const off = registerSettingsLeaveGuard(() => true)
  assert.equal(settingsBlocksLeave(), true)
  off()
  assert.equal(settingsBlocksLeave(), false)
})

test('重複登記以最後一個為準；舊的解除函式不會拆掉新的', () => {
  const offOld = registerSettingsLeaveGuard(() => true)
  const offNew = registerSettingsLeaveGuard(() => false)
  assert.equal(settingsBlocksLeave(), false, '最後登記的算數')
  offOld()
  const offThird = registerSettingsLeaveGuard(() => true)
  offNew()
  assert.equal(settingsBlocksLeave(), true, '舊的解除函式不拆新的登記')
  offThird()
  assert.equal(settingsBlocksLeave(), false)
})
