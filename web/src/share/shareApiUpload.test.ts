/**
 * #1661：分享頁移除正在上傳的附件，中止要一路傳到 fetch（不然本機放掉了、連線還在傳）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { httpShareClient } from './shareApi'

test('upload 把取消訊號交給 fetch', async () => {
  const orig = globalThis.fetch
  let seen: RequestInit | undefined
  globalThis.fetch = (async (_url: unknown, init?: RequestInit) => {
    seen = init
    return new Response('{"id":"a","name":"a.txt"}', { status: 200 })
  }) as typeof fetch
  try {
    const ctl = new AbortController()
    await httpShareClient('tok').upload(new File(['x'], 'a.txt', { type: 'text/plain' }), ctl.signal)
    assert.equal(seen?.signal, ctl.signal)
  } finally {
    globalThis.fetch = orig
  }
})
