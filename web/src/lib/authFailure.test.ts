import test from 'node:test'
import assert from 'node:assert/strict'
import { authActionTargetId, isAuthFailure } from './authFailure.ts'

const sys = (content: string) => ({ role: 'system' as const, content })
const bot = (content: string) => ({ role: 'assistant' as const, content })

test('hook 分類成帳號或授權的失敗收尾算 auth 失敗', () => {
  assert.ok(isAuthFailure(sys('這一回合失敗收尾（帳號或授權）：authentication_failed')))
  assert.ok(isAuthFailure(sys('送出後沒有回應：畫面上是\n⎿  Not logged in · Please run /login')))
  assert.equal(isAuthFailure(sys('這一回合失敗收尾（API 錯誤）：overloaded')), false)
})

test('agent 回覆只有整則就是 Not logged in 那一行才算（兩種寫法）', () => {
  assert.ok(isAuthFailure(bot('Not logged in · Please run /login')))
  assert.ok(isAuthFailure(bot('⎿  Not logged in · Run /login')))
  // 回報裡引用那句話的 bot 是登入好好的（SPEC §18 協調者登入偵測同理）。
  assert.equal(isAuthFailure(bot('協調者畫面是「Not logged in · Please run /login」，我已經請使用者處理。')), false)
  assert.equal(isAuthFailure({ role: 'user', content: 'Not logged in · Please run /login' }), false)
})

test('只掛在最後一則 auth 失敗，後面有正常回覆就不掛', () => {
  const a = { id: 'a', ...sys('這一回合失敗收尾（帳號或授權）：authentication_failed') }
  const b = { id: 'b', ...sys('這一回合失敗收尾（帳號或授權）：authentication_failed') }
  const u = { id: 'u', role: 'user' as const, content: '再試一次' }
  const ok = { id: 'ok', ...bot('好了，改完了。') }
  assert.equal(authActionTargetId([a, u, b]), 'b')
  assert.equal(authActionTargetId([a, u]), 'a', '重送還在跑：按鈕先留著')
  assert.equal(authActionTargetId([a, u, ok]), null)
  assert.equal(authActionTargetId([ok]), null)
  assert.equal(authActionTargetId([]), null)
})
