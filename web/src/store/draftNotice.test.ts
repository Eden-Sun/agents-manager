import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types'
import { draftLimitText, draftMaxBytesOf, draftTooLargeText, useDraftNotice } from './draftNotice'

test('上限數字從 daemon 回應的 max_bytes 取；沒帶或不是正整數就是 null，不在 web 寫死', () => {
  assert.equal(draftMaxBytesOf(new ApiError(400, { error: 'draft_too_large', max_bytes: 262144 }, 'x')), 262144)
  assert.equal(draftMaxBytesOf(new ApiError(400, { error: 'draft_too_large', max_bytes: 1000 }, 'x')), 1000)
  assert.equal(draftMaxBytesOf(new ApiError(413, {}, 'too large')), null, '413 來自 HTTP 層時沒有 JSON 上限')
  assert.equal(draftMaxBytesOf(new ApiError(400, { error: 'draft_too_large', max_bytes: '262144' }, 'x')), null)
  assert.equal(draftMaxBytesOf(new ApiError(400, { error: 'draft_too_large', max_bytes: 0 }, 'x')), null)
  assert.equal(draftMaxBytesOf(new TypeError('Failed to fetch')), null)
})

test('提示文字：有上限就寫出來（KB），沒有就只說太長、沒同步', () => {
  assert.equal(draftLimitText(262144), '256 KB')
  assert.equal(draftTooLargeText(262144), '這份草稿太長（上限 256 KB），沒有同步到其他裝置；內容還留在這個輸入框裡。')
  assert.equal(draftTooLargeText(null), '這份草稿太長，沒有同步到其他裝置；內容還留在這個輸入框裡。')
})

test('store：mark 記下被拒的 key 與上限，clear 只收掉那一個 key', () => {
  // 同一個行程裡別的測試檔也會寫這個 store（例如 shell 草稿），所以只看這裡自己用的 key，不比整張表。
  const { mark, clear } = useDraftNotice.getState()
  mark('bot:a', new ApiError(400, { error: 'draft_too_large', max_bytes: 262144 }, 'x'))
  mark('group:p', new ApiError(413, {}, 'too large'))
  const state = () => useDraftNotice.getState().rejected
  assert.equal(state()['bot:a'], 262144)
  assert.equal(state()['group:p'], null)
  clear('bot:a')
  assert.equal('bot:a' in state(), false)
  assert.equal('group:p' in state(), true, '只收掉自己那一個 key')
  clear('bot:a')
  assert.equal('group:p' in state(), true)
  clear('group:p')
  assert.equal('group:p' in state(), false)
})
