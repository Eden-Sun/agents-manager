import test from 'node:test'
import assert from 'node:assert/strict'
import { ManualClock } from './mockClock.ts'

test('advance 依到期時間、同刻依排入順序執行，沒到期的留著', async () => {
  const clock = new ManualClock()
  const log: string[] = []
  clock.schedule(() => log.push('b@20'), 20)
  clock.schedule(() => log.push('a@10'), 10)
  clock.schedule(() => log.push('c@20'), 20)
  clock.schedule(() => log.push('late@100'), 100)
  await clock.advance(25)
  assert.deepEqual(log, ['a@10', 'b@20', 'c@20'])
  assert.equal(clock.pending, 1)
  assert.equal(clock.now(), 25)
})

test('timer 裡再排的 timer 在同一次 advance 內只要到期就會跑（回合串起來的 2.4 秒一次走完）', async () => {
  const clock = new ManualClock()
  const log: number[] = []
  clock.schedule(() => {
    log.push(clock.now())
    clock.schedule(() => log.push(clock.now()), 500)
  }, 400)
  await clock.advance(1000)
  assert.deepEqual(log, [400, 900])
})

test('取消的 timer 不跑；advance 之間 promise 鏈有機會跑完', async () => {
  const clock = new ManualClock()
  const log: string[] = []
  const id = clock.schedule(() => log.push('cancelled'), 5)
  clock.cancel(id)
  clock.schedule(() => void Promise.resolve().then(() => log.push('microtask')), 10)
  clock.schedule(() => log.push('next'), 11)
  await clock.advance(20)
  assert.deepEqual(log, ['microtask', 'next'], '前一個 timer 的 promise 先跑完才輪到下一個')
})
