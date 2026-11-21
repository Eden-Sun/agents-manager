import test from 'node:test'
import assert from 'node:assert/strict'
import { nestableBusy } from './nestableBusy.ts'

test('nestableBusy: inner end() does not clear busy while an outer call is still in flight', () => {
  const seen: boolean[] = []
  const lock = nestableBusy((b) => seen.push(b))

  lock.begin()
  assert.deepEqual(seen, [true])

  lock.begin()
  lock.end()
  assert.deepEqual(seen, [true], 'busy flipped false while the outer call was still pending')

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
