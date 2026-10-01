import test from 'node:test'
import assert from 'node:assert/strict'
import { scheduleReviewRefresh } from './reviewRefresh.ts'

/**
 * 更新框的「AGM 解析」只在開啟時讀一次：AGM 之後給出結論，框裡一直是「解析中…」。daemon 的交辦轉換會推
 * `supervisor_changed`（store 的 `supervisorRev`），這裡決定哪些情況值得重讀：還沒有結論才讀；已經 `done` 就不再打 API。
 */
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms))

test('還沒有結論（沒派、派了還沒回）：rev 變了就重讀', async () => {
  for (const state of ['none', 'pending', undefined] as const) {
    let runs = 0
    scheduleReviewRefresh({ rev: 3, state, delayMs: 5, run: () => void (runs += 1) })
    await wait(30)
    assert.equal(runs, 1, `state=${String(state)}`)
  }
})

test('已經有結論、或 rev 還是 0（沒收過任何事件）：不讀', async () => {
  let runs = 0
  scheduleReviewRefresh({ rev: 3, state: 'done', delayMs: 5, run: () => void (runs += 1) })
  scheduleReviewRefresh({ rev: 0, state: 'pending', delayMs: 5, run: () => void (runs += 1) })
  await wait(30)
  assert.equal(runs, 0)
})

test('取消（元件卸載、或下一個 rev 來了）就不讀', async () => {
  let runs = 0
  const cancel = scheduleReviewRefresh({ rev: 1, state: 'pending', delayMs: 10, run: () => void (runs += 1) })
  cancel()
  await wait(30)
  assert.equal(runs, 0)
})
