import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createRequestId, settleCreateRequest } from './createRequestId'

test('同一個動作失敗後重試拿到同一個鍵，成功之後才換新的', () => {
  const a = createRequestId('add:p1:claude:cc1')
  assert.equal(createRequestId('add:p1:claude:cc1'), a, '失敗重試沿用同一個鍵（daemon 才認得是同一件事）')
  settleCreateRequest('add:p1:claude:cc1')
  assert.notEqual(createRequestId('add:p1:claude:cc1'), a, '成功之後再點一次是新的動作')
})

test('不同動作各有各的鍵', () => {
  assert.notEqual(createRequestId('add:p1:claude:cc1'), createRequestId('add:p1:claude:cc2'))
  settleCreateRequest('add:p1:claude:cc1')
  settleCreateRequest('add:p1:claude:cc2')
})

test('鍵符合 daemon 收的字元集與長度', () => {
  const id = createRequestId('k')
  assert.match(id, /^[A-Za-z0-9\-_.:]{1,128}$/)
  settleCreateRequest('k')
})

test('帶 maxAgeMs：超過就換新的鍵（舊的重送窗口已過，同一句再送是新的動作）', () => {
  const t0 = 1_000_000
  const a = createRequestId('age:k', { maxAgeMs: 60_000, now: t0 })
  assert.equal(createRequestId('age:k', { maxAgeMs: 60_000, now: t0 + 59_000 }), a, '窗口內沿用')
  const b = createRequestId('age:k', { maxAgeMs: 60_000, now: t0 + 61_000 })
  assert.notEqual(b, a, '超過窗口換新的')
  assert.equal(createRequestId('age:k', { maxAgeMs: 60_000, now: t0 + 62_000 }), b)
  settleCreateRequest('age:k')
})

test('不帶 maxAgeMs：行為跟以前一樣，一直沿用到成功（建 bot 的冪等鍵靠這個）', () => {
  const a = createRequestId('age:legacy', { now: 1 })
  assert.equal(createRequestId('age:legacy', { now: 99_999_999 }), a)
  settleCreateRequest('age:legacy')
})
