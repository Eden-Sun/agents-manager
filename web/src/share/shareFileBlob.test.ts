import test from 'node:test'
import assert from 'node:assert/strict'
import { httpShareClient } from './shareApi.ts'
import { ShareHttpError } from './shareModel.ts'

// #1089：分享入口的下載名額每個分享同時 2 個、不排隊；整檔下載要自己排隊、被擋了照 Retry-After 重試。
const TOKEN = 'TOKEN_abcdefghijklmnop'
const tick = () => new Promise<void>((r) => setTimeout(r, 0))

test('fileBlob 一次只發一個下載', async () => {
  const orig = globalThis.fetch
  let inflight = 0
  let maxInflight = 0
  const urls: string[] = []
  const gates: Array<() => void> = []
  globalThis.fetch = (async (url: string) => {
    urls.push(url)
    inflight++
    maxInflight = Math.max(maxInflight, inflight)
    await new Promise<void>((r) => gates.push(r))
    inflight--
    return new Response('x', { status: 200 })
  }) as unknown as typeof fetch
  try {
    const client = httpShareClient(TOKEN)
    const ps = [client.fileBlob('a.png'), client.fileBlob('b.png'), client.fileBlob('c.png')]
    for (let i = 0; i < 3; i++) {
      while (gates.length < i + 1) await tick()
      gates[i]()
      await ps[i]
    }
    assert.equal(maxInflight, 1, '同時只有一個下載在飛')
    assert.deepEqual(urls.map((u) => decodeURIComponent(u.split('/').pop()!)), ['a.png', 'b.png', 'c.png'])
  } finally {
    globalThis.fetch = orig
  }
})

test('fileBlob：429／503 照 Retry-After 等了重試，30 秒壓到 10 秒', async () => {
  const orig = globalThis.fetch
  const queue = [
    new Response('', { status: 429, headers: { 'Retry-After': '5' } }),
    new Response('', { status: 503, headers: { 'Retry-After': '30' } }),
    new Response('x', { status: 200 }),
  ]
  let calls = 0
  globalThis.fetch = (async () => {
    calls++
    return queue.shift()!
  }) as unknown as typeof fetch
  const sleeps: number[] = []
  try {
    const b = await httpShareClient(TOKEN, { sleep: async (ms) => void sleeps.push(ms) }).fileBlob('a.png')
    assert.ok(b instanceof Blob)
    assert.equal(calls, 3)
    assert.deepEqual(sleeps, [5000, 10000])
  } finally {
    globalThis.fetch = orig
  }
})

test('fileBlob：一直 429 終究放棄，把 429 拋出去（共 BLOB_RETRY_MAX + 1 = 7 次）', async () => {
  const orig = globalThis.fetch
  let calls = 0
  globalThis.fetch = (async () => {
    calls++
    return new Response('', { status: 429, headers: { 'Retry-After': '1' } })
  }) as unknown as typeof fetch
  try {
    await assert.rejects(httpShareClient(TOKEN, { sleep: async () => {} }).fileBlob('a.png'), (e) => e instanceof ShareHttpError && e.status === 429)
    assert.equal(calls, 7)
  } finally {
    globalThis.fetch = orig
  }
})

test('fileBlob：404 與網路錯誤不重試', async () => {
  const orig = globalThis.fetch
  try {
    let calls = 0
    globalThis.fetch = (async () => {
      calls++
      return new Response('', { status: 404 })
    }) as unknown as typeof fetch
    await assert.rejects(httpShareClient(TOKEN, { sleep: async () => {} }).fileBlob('a.png'), (e) => e instanceof ShareHttpError && e.status === 404)
    assert.equal(calls, 1)

    calls = 0
    globalThis.fetch = (async () => {
      calls++
      throw new TypeError('network down')
    }) as unknown as typeof fetch
    await assert.rejects(httpShareClient(TOKEN, { sleep: async () => {} }).fileBlob('a.png'), TypeError)
    assert.equal(calls, 1)
  } finally {
    globalThis.fetch = orig
  }
})

test('fileBlob：前一個失敗不卡住後面的', async () => {
  const orig = globalThis.fetch
  const queue = [new Response('', { status: 404 }), new Response('x', { status: 200 })]
  globalThis.fetch = (async () => queue.shift()!) as unknown as typeof fetch
  try {
    const client = httpShareClient(TOKEN, { sleep: async () => {} })
    const first = client.fileBlob('a.png')
    const second = client.fileBlob('b.png')
    await assert.rejects(first, (e) => e instanceof ShareHttpError && e.status === 404)
    assert.ok((await second) instanceof Blob)
  } finally {
    globalThis.fetch = orig
  }
})
