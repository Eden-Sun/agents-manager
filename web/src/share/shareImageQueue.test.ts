import test from 'node:test'
import assert from 'node:assert/strict'
import { ImageWorkSkipped, ShareImageWorkQueue, SHARE_IMAGE_PENDING_LIMIT } from './shareImageQueue'

test('圖片工作佇列同 key 去重、有界，最近要求的工作先跑', async () => {
  const queue = new ShareImageWorkQueue()
  const started: string[] = []
  let releaseActive!: () => void
  const gate = new Promise<void>((resolve) => (releaseActive = resolve))
  const active = queue.schedule('active', async () => {
    started.push('active')
    await gate
    return 'active'
  })
  await Promise.resolve()

  const queued = Array.from({ length: 100 }, (_, i) => queue.schedule(`image-${i}`, async () => {
    started.push(`image-${i}`)
    return i
  }))
  for (const work of queued) void work.promise.catch(() => {})
  assert.equal(queue.activeCount, 1)
  assert.equal(queue.pendingCount, SHARE_IMAGE_PENDING_LIMIT)
  await assert.rejects(queued[0].promise, (error: unknown) => error instanceof ImageWorkSkipped && error.message === 'image_work_dropped')

  const duplicate = queue.schedule('image-99', async () => {
    started.push('duplicate')
    return -1
  })
  assert.equal(duplicate.promise, queued[99].promise, '同一張圖共用同一個 promise')
  assert.equal(queue.pendingCount, SHARE_IMAGE_PENDING_LIMIT)

  releaseActive()
  await Promise.all([active.promise, ...queued.slice(-SHARE_IMAGE_PENDING_LIMIT).map((work) => work.promise)])
  assert.equal(started[0], 'active')
  assert.equal(started[1], 'image-99', 'LIFO 先跑最後要求的圖片')
  assert.equal(started.length, SHARE_IMAGE_PENDING_LIMIT + 1, '超過上限後丟掉最舊的未開始項')
  assert.equal(started.includes('image-0'), false)
  assert.equal(started.includes('duplicate'), false)
})

test('未開始的圖片工作可以取消，取消後不會執行', async () => {
  const queue = new ShareImageWorkQueue(2)
  let releaseActive!: () => void
  const gate = new Promise<void>((resolve) => (releaseActive = resolve))
  const started: string[] = []
  const active = queue.schedule('active', async () => {
    started.push('active')
    await gate
  })
  await Promise.resolve()
  const stale = queue.schedule('stale', async () => void started.push('stale'))
  void stale.promise.catch(() => {})
  assert.equal(stale.cancel(), true)
  assert.equal(stale.cancel(), false)
  await assert.rejects(stale.promise, (error: unknown) => error instanceof ImageWorkSkipped && error.message === 'image_work_cancelled')
  releaseActive()
  await active.promise
  assert.deepEqual(started, ['active'])
})
