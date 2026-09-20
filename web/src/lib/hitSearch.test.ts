import test from 'node:test'
import assert from 'node:assert/strict'
import { createHitSearch } from './hitSearch.ts'

const tick = () => new Promise((r) => setTimeout(r, 5))

test('請求在飛時清空搜尋框：晚到的舊結果不能寫回去', async () => {
  let resolve: (v: string) => void = () => {}
  const seen: (string | null)[] = []
  const s = createHitSearch<string>(
    () => new Promise((r) => (resolve = r)),
    (h) => seen.push(h),
    0,
  )
  s.run('abc')
  await tick()
  s.run('')
  resolve('舊結果')
  await tick()
  assert.deepEqual(seen, [null], '清空後只該有一次清掉，舊結果不寫')
})

test('改字之後只採最新那次的結果', async () => {
  const resolvers: Record<string, (v: string) => void> = {}
  const seen: (string | null)[] = []
  const s = createHitSearch<string>(
    (q) => new Promise((r) => (resolvers[q] = r)),
    (h) => seen.push(h),
    0,
  )
  s.run('a')
  await tick()
  s.run('ab')
  await tick()
  resolvers['ab']('ab 的結果')
  resolvers['a']('a 的結果')
  await tick()
  assert.deepEqual(seen, ['ab 的結果'])
})
