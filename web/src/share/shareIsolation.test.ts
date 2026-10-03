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
        ['react', 'react-dom/client', 'react-markdown', 'remark-gfm'].includes(spec)
      assert.ok(ok, `${f} import 了 ${spec}：分享頁不能帶主 UI 的程式碼`)
    }
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
  } finally {
    globalThis.fetch = orig
  }
  assert.equal(calls.length, 4)
  for (const { url, init } of calls) {
    assert.ok(url.startsWith('/s/TOKEN_abcdefghijklmnop/api/'), url)
    assert.equal(init.credentials, 'omit')
    assert.equal(init.referrerPolicy, 'no-referrer')
    assert.equal(new Headers(init.headers).get('X-AM-Token'), null)
  }
  assert.deepEqual(JSON.parse(String(calls[1].init.body)), { text: 'hi', client_request_id: 'crid', attachments: ['att1'] })
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
