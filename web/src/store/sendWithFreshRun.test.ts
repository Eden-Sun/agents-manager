import test from 'node:test'
import assert from 'node:assert/strict'
import { sendWithFreshRun } from './store.ts'
import { ApiError } from '../api/types.ts'
import type { StoreState } from './store.ts'

/** 只需要 `runs` 與 `refreshState` 兩樣，其餘不碰。 */
function fakeGet(runId: string | null, onRefresh?: () => void) {
  return () =>
    ({
      runs: runId ? { b1: { id: runId } } : {},
      refreshState: async () => {
        onRefresh?.()
      },
    }) as unknown as StoreState
}

const mismatch = (current: string) => new ApiError(409, { error: 'conflict', reason: 'run mismatch', run_id: current }, 'conflict')

test('平常就是拿快取的 run id 送一次', async () => {
  const seen: (string | null)[] = []
  await sendWithFreshRun(fakeGet('run-1'), 'b1', async (id) => {
    seen.push(id)
  })
  assert.deepEqual(seen, ['run-1'])
})

test('run mismatch：用 409 帶回來的現行 run id 重試一次，使用者不必自己再按', async () => {
  const seen: (string | null)[] = []
  let refreshed = false
  await sendWithFreshRun(fakeGet('stale', () => (refreshed = true)), 'b1', async (id) => {
    seen.push(id)
    if (id === 'stale') throw mismatch('run-now')
  })
  assert.deepEqual(seen, ['stale', 'run-now'])
  assert.equal(refreshed, true, '快取過期了，順手拉一次狀態')
})

test('重試也失敗就原樣拋出，不會無限重試', async () => {
  const seen: (string | null)[] = []
  await assert.rejects(
    sendWithFreshRun(fakeGet('stale'), 'b1', async (id) => {
      seen.push(id)
      throw mismatch('run-now')
    }),
    (e: unknown) => e instanceof ApiError && e.status === 409,
  )
  assert.deepEqual(seen, ['stale', 'run-now'], '只重試一次')
})

test('其他 409（框裡有字、回合在飛）照舊原樣回報，不重試', async () => {
  let calls = 0
  await assert.rejects(
    sendWithFreshRun(fakeGet('run-1'), 'b1', async () => {
      calls += 1
      throw new ApiError(409, { error: 'conflict', reason: 'composer_busy' }, 'conflict')
    }),
    (e: unknown) => e instanceof ApiError && e.body.reason === 'composer_busy',
  )
  assert.equal(calls, 1)
})

test('沒有帶 run_id 的 run mismatch 不重試（沒有新 id 可用）', async () => {
  let calls = 0
  await assert.rejects(
    sendWithFreshRun(fakeGet('run-1'), 'b1', async () => {
      calls += 1
      throw new ApiError(409, { error: 'conflict', reason: 'run mismatch' }, 'conflict')
    }),
    (e: unknown) => e instanceof ApiError,
  )
  assert.equal(calls, 1)
})
