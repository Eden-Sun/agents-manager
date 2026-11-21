import { test } from 'node:test'
import assert from 'node:assert/strict'
import { judgeApiMissing } from './judge'
import { ApiError } from './types'

test('judgeApiMissing：只有 404／405 算舊 daemon', () => {
  assert.equal(judgeApiMissing(new ApiError(404, {}, 'x')), true)
  assert.equal(judgeApiMissing(new ApiError(405, {}, 'x')), true)
})

test('judgeApiMissing：daemon 自己出錯／認證失敗／閘道錯誤都不是「舊 daemon」', () => {
  for (const status of [400, 403, 409, 500, 502, 504]) {
    assert.equal(judgeApiMissing(new ApiError(status, {}, 'x')), false, `status=${status} 不該被當成舊 daemon`)
  }
})

test('judgeApiMissing：連線層的錯誤（不是 ApiError）也不是「舊 daemon」，要往外丟給呼叫端顯示', () => {
  assert.equal(judgeApiMissing(new TypeError('Failed to fetch')), false)
  assert.equal(judgeApiMissing(new Error('boom')), false)
  assert.equal(judgeApiMissing(null), false)
  assert.equal(judgeApiMissing('nope'), false)
})
