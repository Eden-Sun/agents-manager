/**
 * 分享頁的打包邊界（SPEC「分享 bot」威脅模型 1）：拿到連結的人＝網路上任何人，
 * `src/share/` 不准 import 主 UI 的 store／api／components／hooks（那些會帶著管理 API 的路徑與 token 邏輯）。
 * client 只准打 `/s/<token>/api/…`，不帶 cookie。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { httpShareClient } from './shareApi.ts'

const dir = new URL('.', import.meta.url).pathname

test('src/share 只 import 自己資料夾、react、markdown 套件', () => {
  for (const f of readdirSync(dir).filter((x) => /\.tsx?$/.test(x) && !x.includes('.test.'))) {
    const src = readFileSync(join(dir, f), 'utf8')
    for (const m of src.matchAll(/(?:from|import)\s*\(?\s*'([^']+)'/g)) {
      const spec = m[1]
      const ok =
        spec.startsWith('./') ||
        spec === '../lib/markdownGuard' ||
        // 送出的冪等鍵（#921）：純 sessionStorage 的小函式，自己不 import 任何東西（下面的測試守著這點）。
        spec === '../lib/createRequestId' ||
        ['react', 'react-dom/client', 'react-markdown', 'remark-gfm'].includes(spec)
      assert.ok(ok, `${f} import 了 ${spec}：分享頁不能帶主 UI 的程式碼`)
    }
  }
})

test('分享頁可 import 的 ../lib 檔自己不 import 任何東西（不能經它帶進 store／api）', () => {
  for (const name of ['createRequestId.ts']) {
    const src = readFileSync(join(dir, '..', 'lib', name), 'utf8')
    assert.equal(/(?:from|import)\s*\(?\s*'/.test(src), false, `${name} 不能有 import`)
  }
})

test('httpShareClient 只打 /s/<token>/api/…、credentials omit、no-referrer', async () => {
  const calls: { url: string; init: RequestInit }[] = []
  const orig = globalThis.fetch
  globalThis.fetch = (async (url: string, init: RequestInit) => {
    calls.push({ url, init })
    const body = url.includes('/files') ? { files: [] } : url.includes('/upload') ? { id: 'att1', name: 'a.txt' } : { bot_name: 'b', messages: [] }
    return new Response(JSON.stringify(body), { status: 200 })
  }) as typeof fetch
  try {
    const c = httpShareClient('TOKEN_abcdefghijklmnop')
    await c.messages()
    await c.send('hi', 'crid', ['att1'])
    await c.files()
    await c.upload(new File(['x'], 'a.txt'))
    assert.equal(c.fileUrl('../../etc/passwd'), '/s/TOKEN_abcdefghijklmnop/api/files/..%2F..%2Fetc%2Fpasswd', '檔名一律 encode，不能變成路徑')
    await c.messages('m1')
    await c.messages('../other-share')
    await c.messages('id with space')
  } finally {
    globalThis.fetch = orig
  }
  assert.equal(calls.length, 7)
  assert.ok(calls[4].url.includes('before=m1'), calls[4].url)
  assert.equal(calls[5].url.includes('before='), false, calls[5].url)
  assert.equal(calls[6].url.includes('before='), false, calls[6].url)
  for (const { url, init } of calls) {
    assert.ok(url.startsWith('/s/TOKEN_abcdefghijklmnop/api/'), url)
    assert.equal(init.credentials, 'omit')
    assert.equal(init.referrerPolicy, 'no-referrer')
    assert.equal(new Headers(init.headers).get('X-AM-Token'), null)
  }
  assert.deepEqual(JSON.parse(String(calls[1].init.body)), { text: 'hi', client_request_id: 'crid', attachments: ['att1'] })
})

test('SSE 錯誤不會立刻重開一堆連線', () => {
  const opened: { closed: boolean; onerror: (() => void) | null }[] = []
  class Fake {
    onerror: (() => void) | null = null
    onopen: (() => void) | null = null
    closed = false
    constructor(_url: string) {
      opened.push(this)
    }
    addEventListener() {}
    close() {
      this.closed = true
    }
  }
  const orig = globalThis.EventSource
  globalThis.EventSource = Fake as unknown as typeof EventSource
  try {
    const stop = httpShareClient('TOKEN_abcdefghijklmnop').subscribe({ onMessage() {}, onStatus() {}, onDown() {} })
    assert.equal(opened.length, 1)
    for (let i = 0; i < 20; i++) opened[0].onerror?.()
    assert.equal(opened.length, 1, '同一輪 error 不能再 new EventSource')
    assert.equal(opened[0].closed, true)
    stop()
  } finally {
    globalThis.EventSource = orig
  }
})

test('400 帶 reason：client 讀出 unknown_attachment；讀不懂 body 就當沒有原因（#1097）', async () => {
  const orig = globalThis.fetch
  try {
    globalThis.fetch = (async () => new Response(JSON.stringify({ error: 'bad_request', reason: 'unknown_attachment', message: 'x' }), { status: 400 })) as unknown as typeof fetch
    await assert.rejects(httpShareClient('TOKEN_abcdefghijklmnop').send('x', 'c', ['a']), (e: { status: number; reason: string | null }) => e.status === 400 && e.reason === 'unknown_attachment')
    globalThis.fetch = (async () => new Response('not json', { status: 400 })) as unknown as typeof fetch
    await assert.rejects(httpShareClient('TOKEN_abcdefghijklmnop').send('x', 'c', []), (e: { status: number; reason: string | null }) => e.status === 400 && e.reason === null)
  } finally {
    globalThis.fetch = orig
  }
})

test('HTTP 錯誤帶 status 與 Retry-After', async () => {
  const orig = globalThis.fetch
  globalThis.fetch = (async () => new Response('', { status: 429, headers: { 'Retry-After': '12' } })) as unknown as typeof fetch
  try {
    await assert.rejects(httpShareClient('TOKEN_abcdefghijklmnop').send('x', 'c', []), (e: { status: number; retryAfter: number }) => e.status === 429 && e.retryAfter === 12)
  } finally {
    globalThis.fetch = orig
  }
})
