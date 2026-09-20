import { test } from 'node:test'
import assert from 'node:assert/strict'
import { wakeSupport, whyUnavailable } from './wakeLock'

test('有 API 就是可用；沒有的話分成「不是 HTTPS」與「瀏覽器不支援」', () => {
  assert.equal(wakeSupport({ wakeLock: {} }, true), 'ok')
  assert.equal(wakeSupport({}, false), 'insecure', 'http 開的頁面最常見，要指名是網址的問題')
  assert.equal(wakeSupport({}, true), 'unsupported')
})

test('說不出為什麼就等於靜靜失效：兩種原因各有各的話', () => {
  assert.match(whyUnavailable('insecure'), /HTTPS/)
  assert.match(whyUnavailable('unsupported'), /16\.4/)
  assert.notEqual(whyUnavailable('insecure'), whyUnavailable('unsupported'))
})
