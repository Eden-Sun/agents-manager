import assert from 'node:assert/strict'
import test from 'node:test'
import { createResyncRunner } from './resyncQueue'

test('重抓進行中又來 resync：跑完再跑一次，多個合併成一次（#365）', async () => {
  let runs = 0
  let release: () => void = () => {}
  const trigger = createResyncRunner(async () => {
    runs += 1
    await new Promise<void>((r) => (release = r))
  })
  trigger()
  trigger()
  trigger()
  assert.equal(runs, 1)
  release()
  await new Promise((r) => setTimeout(r, 5))
  assert.equal(runs, 2, '第二輪要補跑，而且三個合成一輪')
  release()
  await new Promise((r) => setTimeout(r, 5))
  trigger()
  assert.equal(runs, 3, '都跑完之後新的 resync 照常起跑')
  release()
})

test('某一輪丟例外也不卡住：下一個 resync 還能跑', async () => {
  let runs = 0
  const trigger = createResyncRunner(async () => {
    runs += 1
    throw new Error('x')
  })
  trigger()
  await new Promise((r) => setTimeout(r, 5))
  trigger()
  assert.equal(runs, 2)
})
