import test from 'node:test'
import assert from 'node:assert/strict'
import { nestableBusy } from './nestableBusy.ts'

test('nestableBusy: inner end() does not clear busy while an outer call is still in flight', () => {
  const seen: boolean[] = []
  const lock = nestableBusy((b) => seen.push(b))

  // 外層（例如「測試連線」）開始
  lock.begin()
  assert.deepEqual(seen, [true])

  // 裡層（「測試連線」內部借用的「儲存」）整段跑完，自己也 begin/end 一次
  lock.begin()
  lock.end()
  // 外層還沒做完：busy 不能被裡層的 end() 撥回 false
  assert.deepEqual(seen, [true], 'busy flipped false while the outer call was still pending')

  // 外層真的做完了才會撥回 false
  lock.end()
  assert.deepEqual(seen, [true, false])
})

test('nestableBusy: a single (non-nested) call still toggles busy true then false', () => {
  const seen: boolean[] = []
  const lock = nestableBusy((b) => seen.push(b))
  lock.begin()
  lock.end()
  assert.deepEqual(seen, [true, false])
})

test('nestableBusy: end() without a matching begin() does not go negative or misfire', () => {
  const seen: boolean[] = []
  const lock = nestableBusy((b) => seen.push(b))
  lock.end()
  assert.deepEqual(seen, [], 'no begin() happened, so busy should never have been set')
  lock.begin()
  lock.end()
  assert.deepEqual(seen, [true, false])
})
