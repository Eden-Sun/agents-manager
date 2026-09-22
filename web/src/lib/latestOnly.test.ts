import test from 'node:test'
import assert from 'node:assert/strict'
import { createLatestOnly } from './latestOnly.ts'

test('後發的請求先回、先發的晚到：只有最後一張票算數', () => {
  const l = createLatestOnly()
  const first = l.begin()
  const second = l.begin()
  assert.equal(l.isCurrent(first), false)
  assert.equal(l.isCurrent(second), true)
})

test('較舊的非同步快照晚回時，不會覆蓋較新的快照', async () => {
  const l = createLatestOnly()
  let applied = ''
  let finishOld!: (value: string) => void

  const oldRequest = (async () => {
    const request = l.begin()
    const value = await new Promise<string>((resolve) => {
      finishOld = resolve
    })
    if (l.isCurrent(request)) applied = value
  })()

  const freshRequest = l.begin()
  if (l.isCurrent(freshRequest)) applied = 'fresh snapshot'
  finishOld('stale snapshot')
  await oldRequest

  assert.equal(applied, 'fresh snapshot')
})
