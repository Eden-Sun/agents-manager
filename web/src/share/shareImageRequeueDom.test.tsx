import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import { SHARE_IMAGE_REQUEUE_MS } from './ShareImages'
import type { ShareClient } from './shareApi'
import type { ShareFile } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('30 張圖片最後每張都有分享鈕，且每個檔案只抓一次', async () => {
  const calls: string[] = []
  const files: ShareFile[] = Array.from({ length: 30 }, (_, i) => ({
    name: `image-${i}.png`, size: 1, modified_at: null, version: 'v1',
  }))
  const client: ShareClient = {
    messages: async () => ({ bot_name: 'share-test', status: 'idle', messages: [], has_more: false }),
    send: async () => {},
    upload: async () => ({ id: 'upload', name: 'upload.png' }),
    files: async () => files,
    fileUrl: (name) => `/files/${name}`,
    previewUrl: (name) => `/files/${name}?inline=1`,
    fileBlob: async (name) => {
      calls.push(name)
      return new Blob(['image'], { type: 'image/png' })
    },
    subscribe: () => () => {},
  }

  const realSetTimeout = globalThis.setTimeout
  const realClearTimeout = globalThis.clearTimeout
  const realWindowSetTimeout = window.setTimeout
  const realWindowClearTimeout = window.clearTimeout
  const retries: Array<{ handle: ReturnType<typeof setTimeout>; callback: () => void; cancelled: boolean }> = []
  const fakeHandles = new Set<ReturnType<typeof setTimeout>>()
  let scheduled = 0
  let fired = 0
  globalThis.setTimeout = ((handler: TimerHandler, timeout?: number, ...args: unknown[]) => {
    if (timeout === SHARE_IMAGE_REQUEUE_MS && typeof handler === 'function') {
      scheduled++
      const handle = {} as ReturnType<typeof setTimeout>
      fakeHandles.add(handle)
      retries.push({ handle, callback: () => { fired++; handler(...args) }, cancelled: false })
      return handle
    }
    return realSetTimeout(handler, timeout, ...args)
  }) as typeof setTimeout
  const fakeClearTimeout = ((handle?: ReturnType<typeof setTimeout>) => {
    if (handle && fakeHandles.delete(handle)) {
      const retry = retries.find((item) => item.handle === handle)
      if (retry) retry.cancelled = true
      return
    }
    if (handle) realClearTimeout(handle)
  }) as typeof clearTimeout
  globalThis.clearTimeout = fakeClearTimeout
  window.setTimeout = globalThis.setTimeout
  window.clearTimeout = fakeClearTimeout

  try {
    await mount(<ShareApp client={client} />)
    for (let turn = 0; turn < 40; turn++) {
      await act(async () => {
        for (let i = 0; i < 80; i++) await Promise.resolve()
      })
      const due = retries.splice(0).filter((retry) => !retry.cancelled)
      if (due.length) {
        await act(async () => {
          for (const retry of due) retry.callback()
          for (let i = 0; i < 80; i++) await Promise.resolve()
        })
        continue
      }
      if (document.querySelectorAll('.sh-files .sh-png-btn:not([disabled])').length === files.length) break
    }

    assert.equal(document.querySelectorAll('.sh-files .sh-png-btn:not([disabled])').length, files.length, `calls=${calls.length} scheduled=${scheduled} fired=${fired}`)
    assert.equal(document.querySelectorAll('.sh-png-fixing').length, 0)
    assert.ok(scheduled > 0, '佇列略過時會排定重試')
    assert.ok(fired > 0, '虛擬重試計時器確實執行')
    assert.equal(calls.length, files.length)
    assert.equal(new Set(calls).size, files.length, '每個檔名只抓一次')
  } finally {
    await unmountAll()
    globalThis.setTimeout = realSetTimeout
    globalThis.clearTimeout = realClearTimeout
    window.setTimeout = realWindowSetTimeout
    window.clearTimeout = realWindowClearTimeout
  }
})
