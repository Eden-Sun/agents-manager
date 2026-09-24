import test from 'node:test'
import assert from 'node:assert/strict'
import { HttpTransport, isAbortError, xhrUpload } from './transport'
import { ApiError } from './types'

/** 夠 `xhrUpload` 用的假 XHR：記下 header、送出、中止，測試手動觸發進度與回應。 */
class FakeXhr {
  static last: FakeXhr | null = null
  headers: Record<string, string> = {}
  method = ''
  url = ''
  sent: unknown = undefined
  aborted = false
  status = 0
  responseText = ''
  upload: { onprogress: ((e: ProgressEvent) => void) | null } = { onprogress: null }
  onload: (() => void) | null = null
  onerror: (() => void) | null = null
  onabort: (() => void) | null = null
  constructor() {
    FakeXhr.last = this
  }
  open(method: string, url: string) {
    this.method = method
    this.url = url
  }
  setRequestHeader(k: string, v: string) {
    this.headers[k] = v
  }
  send(body: unknown) {
    this.sent = body
  }
  abort() {
    this.aborted = true
    this.onabort?.()
  }
  progress(loaded: number, total: number) {
    this.upload.onprogress?.({ loaded, total, lengthComputable: true } as ProgressEvent)
  }
  respond(status: number, text: string) {
    this.status = status
    this.responseText = text
    this.onload?.()
  }
}

const make = () => new FakeXhr() as unknown as XMLHttpRequest
const blob = new Blob(['hello'], { type: 'application/zip' })

test('xhrUpload：帶 token 與 Content-Type、回報進度、解析 JSON 回應', async () => {
  const seen: Array<[number, number]> = []
  const p = xhrUpload('/api/bots/b/attachments?name=a.zip', blob, 'tok', { onProgress: (l, t) => seen.push([l, t]) }, make)
  const x = FakeXhr.last!
  assert.equal(x.method, 'POST')
  assert.equal(x.headers['X-AM-Token'], 'tok')
  assert.equal(x.headers['Content-Type'], 'application/zip')
  assert.equal(x.sent, blob)
  x.progress(2, 5)
  x.progress(5, 5)
  x.respond(200, '{"id":"att1"}')
  assert.deepEqual(await p, { status: 200, body: { id: 'att1' } })
  assert.deepEqual(seen, [[2, 5], [5, 5]])
})

test('xhrUpload：signal 中止會真的 xhr.abort()，並以 AbortError reject', async () => {
  const ctl = new AbortController()
  const p = xhrUpload('/u', blob, 'tok', { signal: ctl.signal }, make)
  const x = FakeXhr.last!
  x.progress(1, 5)
  ctl.abort()
  assert.equal(x.aborted, true)
  await assert.rejects(p, (e: unknown) => isAbortError(e))
})

test('xhrUpload：已經中止的 signal 連請求都不開', async () => {
  const ctl = new AbortController()
  ctl.abort()
  FakeXhr.last = null
  await assert.rejects(xhrUpload('/u', blob, 'tok', { signal: ctl.signal }, make), (e: unknown) => isAbortError(e))
  assert.equal(FakeXhr.last, null)
})

test('xhrUpload：網路錯誤 reject 一般 Error（不是 AbortError）', async () => {
  const p = xhrUpload('/u', blob, '', {}, make)
  const x = FakeXhr.last!
  assert.equal(x.headers['X-AM-Token'], undefined)
  x.onerror?.()
  await assert.rejects(p, (e: unknown) => e instanceof Error && !isAbortError(e) && /連線中斷/.test(e.message))
})

test('HttpTransport.upload：非 2xx 照舊丟 ApiError（帶 daemon 的原因）', async () => {
  const g = globalThis as { XMLHttpRequest?: unknown }
  const prev = g.XMLHttpRequest
  g.XMLHttpRequest = FakeXhr
  try {
    const t = new HttpTransport()
    const p = t.upload('/bots/b/attachments?name=a.zip', blob)
    FakeXhr.last!.respond(413, '{"error":"bad_request","message":"檔案超過 50 MB"}')
    await assert.rejects(p, (e: unknown) => e instanceof ApiError && e.status === 413 && /50 MB/.test(e.message))
  } finally {
    g.XMLHttpRequest = prev
  }
})
