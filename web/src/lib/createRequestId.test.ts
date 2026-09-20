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
