import test from 'node:test'
import assert from 'node:assert/strict'
import { loadReviewOnOpen } from './autoReview'
import type * as api from '../api'

test('開框 GET 後才明確 POST，並重讀占位；重開由 daemon 去重', async () => {
  const calls: string[] = []
  const client = {
    fetchUpdateReview: async () => { calls.push('GET'); return { version: '2.0', auto: true, review: { state: 'none' } } },
    requestUpdateReview: async (b: { auto?: boolean }) => { assert.equal(b.auto, true); calls.push('POST') },
  } as unknown as Pick<typeof api, 'fetchUpdateReview' | 'requestUpdateReview'>
  await loadReviewOnOpen({ kind: 'herdr', host: 'local', from: '1.0', to: '2.0' }, client)
  assert.deepEqual(calls, ['GET', 'POST', 'GET'])
})

test('停用／API 未就緒或執行中時不自動送出', async () => {
  for (const [auto, state] of [[false, 'none'], [true, 'pending']] as const) {
    const client = {
      fetchUpdateReview: async () => ({ version: '2.0', auto, review: { state } }),
      requestUpdateReview: async () => { throw new Error('must not POST') },
    } as unknown as Pick<typeof api, 'fetchUpdateReview' | 'requestUpdateReview'>
    const result = await loadReviewOnOpen({ kind: 'herdr', host: 'local', from: null, to: null }, client)
    assert.equal(result.review.state, state)
  }
})

test('failed 開框仍明確 POST，由 daemon 指紋判定不自動重試', async () => {
  let posted = 0
  const client = {
    fetchUpdateReview: async () => ({ version: '2.0', auto: true, review: { state: 'failed' } }),
    requestUpdateReview: async () => { posted += 1 },
  } as unknown as Pick<typeof api, 'fetchUpdateReview' | 'requestUpdateReview'>
  await loadReviewOnOpen({ kind: 'herdr', host: 'local', from: null, to: null }, client)
  assert.equal(posted, 1)
})
