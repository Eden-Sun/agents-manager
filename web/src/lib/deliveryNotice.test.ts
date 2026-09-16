import test from 'node:test'
import assert from 'node:assert/strict'
import type { Turn } from '../api/types.ts'
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
