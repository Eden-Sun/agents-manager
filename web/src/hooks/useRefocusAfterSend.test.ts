import test from 'node:test'
import assert from 'node:assert/strict'
import { shouldRefocusAfterSend } from './useRefocusAfterSend.ts'

const base = { wasSending: true, sending: false, hadFocus: true, focusLost: true, phone: false }

test('送出結束、送出前焦點在輸入框、現在掉到 body：把焦點還回輸入框', () => {
  assert.equal(shouldRefocusAfterSend(base), true)
})

test('送出前焦點本來就不在輸入框（使用者點了別處）：不搶焦點', () => {
  assert.equal(shouldRefocusAfterSend({ ...base, hadFocus: false }), false)
})

test('送出期間使用者把焦點移到別的控制項（焦點沒掉到 body）：不搶', () => {
  assert.equal(shouldRefocusAfterSend({ ...base, focusLost: false }), false)
})

test('手機不還焦點（不彈鍵盤）；還在送出中也不動', () => {
  assert.equal(shouldRefocusAfterSend({ ...base, phone: true }), false)
  assert.equal(shouldRefocusAfterSend({ ...base, sending: true }), false)
  assert.equal(shouldRefocusAfterSend({ ...base, wasSending: false }), false)
})
