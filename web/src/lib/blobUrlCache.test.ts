/** 有使用者計數的 blob URL 快取（#2074）：還有人持有的 URL 不因擠出而 revoke。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { createBlobUrlCache } from './blobUrlCache.ts'

const revoked: string[] = []
URL.revokeObjectURL = (u: string) => {
  revoked.push(u)
}
const flush = () => new Promise((r) => setTimeout(r, 0))

test('超過上限時持有中的不擠出；放掉後才依最久沒用的順序 revoke', async () => {
  revoked.length = 0
  const c = createBlobUrlCache(2)
  const a = c.acquire('a', async () => 'blob:a')
  const b = c.acquire('b', async () => 'blob:b')
  const d = c.acquire('d', async () => 'blob:d')
  assert.deepEqual(await Promise.all([a.url, b.url, d.url]), ['blob:a', 'blob:b', 'blob:d'])
  await flush()
  assert.deepEqual(revoked, [])
  a.release()
  a.release() // 重複 release 不能多扣
  b.release()
  await flush()
  assert.deepEqual(revoked, [], '沒人用的 2 筆在上限內')
  d.release()
  await flush()
  assert.deepEqual(revoked, ['blob:a'])
  assert.equal(c.size(), 2)
})

test('同 key 兩個使用者：一個放掉不影響另一個；命中算最近用過', async () => {
  revoked.length = 0
  const c = createBlobUrlCache(1)
  let loads = 0
  const load = async () => `blob:x${++loads}`
  const x1 = c.acquire('x', load)
  c.acquire('y', async () => 'blob:y').release()
  const x2 = c.acquire('x', load) // 命中：x 變成比 y 新
  assert.equal(x1.url, x2.url)
  assert.equal(loads, 1)
  x1.release()
  await flush()
  assert.deepEqual(revoked, [], 'x 還有人持有；y 一筆在上限內')
  x2.release()
  await flush()
  assert.deepEqual(revoked, ['blob:y'], 'x 最近用過，擠掉的是 y')
})

test('失敗的不留在快取；clear 之後舊租約 release 不影響新請求', async () => {
  revoked.length = 0
  const c = createBlobUrlCache(0)
  const bad = c.acquire('k', () => Promise.reject(new Error('net')))
  await bad.url.catch(() => {})
  assert.equal(c.size(), 0)
  const old = c.acquire('k', async () => 'blob:old')
  c.clear()
  const fresh = c.acquire('k', async () => 'blob:new')
  old.release()
  await flush()
  assert.deepEqual(revoked, ['blob:old'])
  assert.equal(c.size(), 1)
  fresh.release()
  await flush()
  assert.deepEqual(revoked, ['blob:old', 'blob:new'])
})
