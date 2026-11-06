/**
 * 圖檔預覽快取：舊請求晚到的失敗不能把同 key 的新請求從快取刪掉（否則重抓一次、新 blob URL 洩漏）。
 * 載入函式由測試注入（`blobFor` 的第 4 個參數），每個請求由測試決定何時 resolve／reject。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { blobFor, clearOutboxPreviews } from './outboxPreviewCache.ts'

type Pending = { resolve: (u: string) => void; reject: (e: Error) => void }
const requests: Pending[] = []
const revoked: string[] = []
const load = () =>
  new Promise<string>((resolve, reject) => {
    requests.push({ resolve, reject })
  })

URL.revokeObjectURL = (u: string) => {
  revoked.push(u)
}

const flush = () => new Promise((r) => setTimeout(r, 0))

test('舊請求在 clear 之後才失敗，不會刪掉同 key 的新快取', async () => {
  requests.length = 0
  revoked.length = 0
  clearOutboxPreviews()

  const old = blobFor('bot', 'a.png', 1, load).url
  clearOutboxPreviews()
  const current = blobFor('bot', 'a.png', 1, load).url
  assert.equal(requests.length, 2)

  requests[0].reject(new Error('old request failed'))
  await old.catch(() => {})
  await flush()

  const again = blobFor('bot', 'a.png', 1, load).url
  assert.equal(again, current, '新 promise 應仍在快取，不該再發請求')
  assert.equal(requests.length, 2, '總請求數應為 2')

  requests[1].resolve('blob:current')
  assert.equal(await again, 'blob:current')

  clearOutboxPreviews()
  await flush()
  assert.ok(revoked.includes('blob:current'), 'clear 應 revoke 新的 blob URL')
})

test('單純失敗仍會從快取移除，下次滑過重抓', async () => {
  requests.length = 0
  clearOutboxPreviews()

  const first = blobFor('bot', 'b.png', 2, load).url
  requests[0].reject(new Error('network'))
  await first.catch(() => {})
  await flush()

  blobFor('bot', 'b.png', 2, load)
  assert.equal(requests.length, 2, '失敗後應重新抓一次')
  requests[1].resolve('blob:retry')
  clearOutboxPreviews()
})

test('一次掛上超過 12 張：每一張拿到的 URL 在畫出來之前都沒被 revoke（#2074）', async () => {
  clearOutboxPreviews()
  revoked.length = 0
  const leases = Array.from({ length: 15 }, (_, i) =>
    blobFor('bot', `shot-${i + 1}.png`, 1, async (_b, name) => `blob:${name}`),
  )
  const got = await Promise.all(leases.map((l) => l.url))
  await flush()
  assert.equal(got.length, 15)
  assert.deepEqual(got.filter((u) => revoked.includes(u)), [], '還在用的 URL 不能被 revoke')

  // 全部卸掉後，沒人用的只留 12 張，最久沒用的 3 張才 revoke。
  for (const l of leases) l.release()
  await flush()
  assert.deepEqual(revoked.sort(), ['blob:shot-1.png', 'blob:shot-2.png', 'blob:shot-3.png'])
  clearOutboxPreviews()
})
