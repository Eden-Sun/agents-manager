import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types.ts'
import { GONE_LABEL, loadFailure, failureLabel } from './attachmentLoad.ts'

test('404／410＝檔案已清除；其他失敗是暫時的', () => {
  assert.equal(loadFailure(new ApiError(404, { error: 'not_found', what: 'attachment' }, 'x')), 'gone')
  assert.equal(loadFailure(new ApiError(410, {}, 'x')), 'gone')
  assert.equal(loadFailure(new ApiError(500, {}, 'x')), 'error')
  assert.equal(loadFailure(new ApiError(401, {}, 'x')), 'error')
  assert.equal(loadFailure(new TypeError('Failed to fetch')), 'error')
})

test('標籤：已清除與暫時失敗講的不一樣，都不是「載入中」', () => {
  assert.equal(failureLabel('gone'), GONE_LABEL)
  assert.notEqual(failureLabel('error'), GONE_LABEL)
  for (const f of ['gone', 'error'] as const) assert.ok(!/載入中|…/.test(failureLabel(f)), f)
})
