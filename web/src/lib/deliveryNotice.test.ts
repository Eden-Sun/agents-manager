import test from 'node:test'
import assert from 'node:assert/strict'
import type { Turn } from '../api/types.ts'
import { toTurn } from '../api/normalize.ts'
import { deliveryNotice } from './deliveryNotice.ts'

const turn = (p: Partial<Turn>) => ({ delivery: 'ok', unverified: false, autoResend: true, ...p }) as Turn

test('有證據就不標', () => {
  assert.equal(deliveryNotice(turn({ unverified: false })), 'none')
})

test('沒有證據但會自動重送：只放 hover，不給使用者層級警示', () => {
  assert.equal(deliveryNotice(turn({ unverified: true, autoResend: true })), 'hint')
})

test('沒有證據又不會重送：這種才標「未驗證送達」', () => {
  assert.equal(deliveryNotice(turn({ unverified: true, autoResend: false })), 'warn')
})

test('unknown／failed 由 delivery 自己顯示，不走這條；沒有 turn 也不標', () => {
  assert.equal(deliveryNotice(turn({ delivery: 'unknown', unverified: true, autoResend: false })), 'none')
  assert.equal(deliveryNotice(turn({ delivery: 'failed', unverified: true, autoResend: false })), 'none')
  assert.equal(deliveryNotice(undefined), 'none')
})

// issue #1471：遠端 bot 補送過一次（auto_resend=0），之後 hook 回報並完成。daemon 在 hook 收尾那一刻把
// delivery_verified 寫成 1，徽章跟著撤掉；沒完成或備援收尾沒有 hook 證據的照舊標。
const wire = (p: Record<string, unknown>) =>
  toTurn({ id: 't1', conversation_id: 'c1', origin: 'web', status: 'in_flight', delivery: 'ok', delivery_verified: 0, auto_resend: 0, resend_count: 1, ...p })

test('補送過一次、hook 回報並完成後 daemon 寫成已驗證：不再標', () => {
  assert.equal(deliveryNotice(wire({ status: 'completed', delivery_verified: 1, native_turn_id: 'n1' })), 'none')
})

test('還沒完成、或備援收尾沒有 hook 證據的：照舊標「未驗證送達」', () => {
  assert.equal(deliveryNotice(wire({ status: 'in_flight' })), 'warn')
  assert.equal(deliveryNotice(wire({ status: 'completed_fallback' })), 'warn')
})
