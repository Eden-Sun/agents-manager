import test from 'node:test'
import assert from 'node:assert/strict'
import { httpShareClient, ShareTimeoutError } from './shareApi.ts'
import { ShareHttpError } from './shareModel.ts'

// #1804：分享頁的 GET／檔案下載要有期限（headers 加讀完 body）；第一張卡住不能堵住後面所有圖。
// 計時器是手動觸發的，不靠牆鐘。
const TOKEN = 'TOKEN_abcdefghijklmnop'
const tick = () => new Promise<void>((r) => setTimeout(r, 0))
const hang = () => new Promise<never>(() => {})
/** body 永遠不來（headers 已回）：讀 body 會一直等。 */
const stalled = () => new Response(new ReadableStream<Uint8Array>({}), { status: 200 })
/** 手動推資料的 body，模擬慢但持續有資料進來的大檔。 */
function controlledBody() {
  let ctl!: ReadableStreamDefaultController<Uint8Array>
  const body = new ReadableStream<Uint8Array>({ start: (c) => { ctl = c } })
  const enc = new TextEncoder()
  return { body, push: (s: string) => ctl.enqueue(enc.encode(s)), close: () => ctl.close() }
}

function fakeTimers() {
  let next = 1
  const live = new Map<number, () => void>()
  return {
    setTimeout: (fn: () => void) => {
      const id = next++
      live.set(id, fn)
      return id
    },
    clearTimeout: (id: unknown) => void live.delete(id as number),
    pending: () => live.size,
    live: () => new Set(live.keys()),
    fireAll: () => {
      for (const [id, fn] of [...live]) {
        live.delete(id)
        fn()
      }
    },
  }
}

/** 等第一個期限計時器登記上去；沒有登記就丟錯，不要空轉到逾時。 */
async function waitForDeadline(timers: ReturnType<typeof fakeTimers>) {
  for (let i = 0; timers.pending() === 0; i++) {
    if (i > 50) throw new Error('沒有任何請求期限被登記')
    await tick()
  }
}

test('GET messages：body 讀到一半卡住，期限到就拋逾時，計時器清掉', async () => {
  const orig = globalThis.fetch
  const timers = fakeTimers()
  globalThis.fetch = (async () => stalled()) as unknown as typeof fetch
  try {
    const p = httpShareClient(TOKEN, { timers }).messages()
    await waitForDeadline(timers)
    timers.fireAll()
    await assert.rejects(p, ShareTimeoutError)
    assert.equal(timers.pending(), 0)
  } finally {
    globalThis.fetch = orig
  }
})

test('fileBlob：第一張 headers 永不回，期限到後第二張照樣發出並拿到', async () => {
  const orig = globalThis.fetch
  const timers = fakeTimers()
  const urls: string[] = []
  globalThis.fetch = (async (url: string) => {
    urls.push(url)
    return url.includes('a.png') ? hang() : new Response('x', { status: 200 })
  }) as unknown as typeof fetch
  try {
    const c = httpShareClient(TOKEN, { timers })
    const a = c.fileBlob('a.png').then(() => null, (e: unknown) => e)
    const b = c.fileBlob('b.png')
    await waitForDeadline(timers)
    timers.fireAll()
    assert.ok((await a) instanceof ShareTimeoutError, '第一張要以逾時結束')
    assert.ok((await b) instanceof Blob, '第二張要拿得到')
    assert.deepEqual(urls.map((u) => decodeURIComponent(u.split('/').pop()!)), ['a.png', 'b.png'])
    assert.equal(timers.pending(), 0)
  } finally {
    globalThis.fetch = orig
  }
})

test('fileBlob：headers 已回、body 讀到一半卡住，期限到也會放行下一張', async () => {
  const orig = globalThis.fetch
  const timers = fakeTimers()
  const urls: string[] = []
  globalThis.fetch = (async (url: string) => {
    urls.push(url)
    if (url.includes('a.png')) return stalled()
    return new Response('x', { status: 200 })
  }) as unknown as typeof fetch
  try {
    const c = httpShareClient(TOKEN, { timers })
    const a = c.fileBlob('a.png').then(() => null, (e: unknown) => e)
    const b = c.fileBlob('b.png')
    await waitForDeadline(timers)
    timers.fireAll()
    assert.ok((await a) instanceof ShareTimeoutError)
    assert.ok((await b) instanceof Blob)
    assert.equal(urls.length, 2)
  } finally {
    globalThis.fetch = orig
  }
})

test('fileBlob 正常下載：拿得到，期限計時器清掉、不留著', async () => {
  const orig = globalThis.fetch
  const timers = fakeTimers()
  globalThis.fetch = (async () => new Response('png-bytes', { status: 200 })) as unknown as typeof fetch
  try {
    const b = await httpShareClient(TOKEN, { timers }).fileBlob('a.png')
    assert.ok(b instanceof Blob)
    assert.equal(await b.text(), 'png-bytes')
    assert.equal(timers.pending(), 0)
  } finally {
    globalThis.fetch = orig
  }
})

test('POST 不重送：upload 回 503 只發一次，拋出原本的 ShareHttpError', async () => {
  const orig = globalThis.fetch
  let calls = 0
  globalThis.fetch = (async () => {
    calls++
    return new Response('', { status: 503, headers: { 'Retry-After': '1' } })
  }) as unknown as typeof fetch
  try {
    const f = new File(['x'], 'a.txt')
    await assert.rejects(httpShareClient(TOKEN, { sleep: async () => {} }).upload(f), (e) => e instanceof ShareHttpError && e.status === 503)
    assert.equal(calls, 1)
  } finally {
    globalThis.fetch = orig
  }
})

test('fileBlob：body 慢但持續有資料進來，每塊都重設閒置計時器，不會因為整段時間長而逾時', async () => {
  const orig = globalThis.fetch
  const timers = fakeTimers()
  const b = controlledBody()
  globalThis.fetch = (async () => new Response(b.body, { status: 200, headers: { 'Content-Type': 'image/png' } })) as unknown as typeof fetch
  try {
    const p = httpShareClient(TOKEN, { timers }).fileBlob('big.png')
    await waitForDeadline(timers)
    for (const part of ['aa', 'bb', 'cc', 'dd']) {
      const before = timers.live()
      b.push(part)
      await tick()
      const after = timers.live()
      assert.equal(after.size, 1, '收到新資料後只留一個閒置計時器')
      assert.ok([...before].every((id) => !after.has(id)), '上一個閒置計時器要清掉，改換新的')
    }
    b.close()
    const blob = await p
    assert.equal(await blob.text(), 'aabbccdd')
    assert.equal(blob.type, 'image/png')
    assert.equal(timers.pending(), 0)
  } finally {
    globalThis.fetch = orig
  }
})

test('files：JSON 的 body 分批慢慢來，持續有資料就不逾時，解析照樣正確', async () => {
  const orig = globalThis.fetch
  const timers = fakeTimers()
  const b = controlledBody()
  globalThis.fetch = (async () => new Response(b.body, { status: 200 })) as unknown as typeof fetch
  try {
    const p = httpShareClient(TOKEN, { timers }).files()
    await waitForDeadline(timers)
    for (const part of ['{"files":[', '{"name":"a.png"}', ']}']) {
      const before = timers.live()
      b.push(part)
      await tick()
      assert.ok([...before].every((id) => !timers.live().has(id)), '每塊資料都要重設閒置計時器')
    }
    b.close()
    assert.deepEqual((await p).map((f) => f.name), ['a.png'])
    assert.equal(timers.pending(), 0)
  } finally {
    globalThis.fetch = orig
  }
})
