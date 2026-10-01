import test from 'node:test'
import assert from 'node:assert/strict'
import { REMOTE_IMAGE_MAX_BYTES, fetchRemoteImageBlob, isRemoteHttpImage, remoteImageInfo, safeImageType } from './remoteImage.ts'

test('只有 http(s) 圖片算遠端；本機路徑、data:、blob: 不算', () => {
  assert.equal(isRemoteHttpImage('https://example.com/a.png'), true)
  assert.equal(isRemoteHttpImage('HTTP://example.com/a.png'), true)
  for (const s of ['docs/a.png', '/Users/x/a.png', 'file:///a.png', 'data:image/png;base64,AAAA', 'blob:http://x/1', '', '//evil.example/a.png']) {
    assert.equal(isRemoteHttpImage(s), false, s)
  }
})

test('placeholder 要講網域；網址帶很長的參數（可能夾帶資料）要警告', () => {
  const plain = remoteImageInfo('https://img.shields.io/badge/ok-green')
  assert.equal(plain?.host, 'img.shields.io')
  assert.equal(plain?.suspicious, false)
  const leak = remoteImageInfo('https://example.invalid/collect?d=' + 'x'.repeat(120))
  assert.equal(leak?.host, 'example.invalid')
  assert.equal(leak?.suspicious, true)
  assert.equal(remoteImageInfo('https://user:pw@example.com/a.png')?.suspicious, true, '帶帳密的網址一律可疑')
  assert.equal(remoteImageInfo('not a url'), null)
  assert.equal(remoteImageInfo('docs/a.png'), null)
})

const fakeFetch = (
  res: { ok?: boolean; status?: number; type?: string; size?: number; length?: string; chunks?: number[]; cancelled?: { v: boolean } },
  seen: { init?: RequestInit }[] = [],
) =>
  (async (_url: string, init?: RequestInit) => {
    seen.push({ init })
    const chunks = res.chunks
    return {
      ok: res.ok ?? true,
      status: res.status ?? 200,
      headers: {
        get: (k: string) => (k.toLowerCase() === 'content-type' ? (res.type ?? 'image/png') : k.toLowerCase() === 'content-length' ? (res.length ?? null) : null),
      },
      blob: async () => new Blob([new Uint8Array(res.size ?? 4)], { type: res.type ?? 'image/png' }),
      body: chunks
        ? {
            getReader: () => {
              let i = 0
              return {
                read: async () => (i < chunks.length ? { done: false, value: new Uint8Array(chunks[i++]) } : { done: true, value: undefined }),
                cancel: async () => {
                  if (res.cancelled) res.cancelled.v = true
                },
              }
            },
          }
        : null,
    } as unknown as Response
  }) as unknown as typeof fetch

test('點了才載入：不帶 cookie、不帶 Referer，要是 image/* 且不超過上限', async () => {
  const seen: { init?: RequestInit }[] = []
  const blob = await fetchRemoteImageBlob('https://example.com/a.png', fakeFetch({}, seen))
  assert.equal(blob.size, 4)
  assert.equal(seen[0].init?.credentials, 'omit')
  assert.equal(seen[0].init?.referrerPolicy, 'no-referrer')
  await assert.rejects(fetchRemoteImageBlob('https://example.com/a', fakeFetch({ type: 'text/html' })), /不是圖片/)
  await assert.rejects(fetchRemoteImageBlob('https://example.com/a', fakeFetch({ ok: false, status: 404 })), /404/)
  await assert.rejects(fetchRemoteImageBlob('https://example.com/a', fakeFetch({ size: REMOTE_IMAGE_MAX_BYTES + 1 })), /太大/)
})

// ── #764 對抗式審查 ──

test('SVG 不能收：blob 在新分頁（<a target=_blank>）開啟時會在 app 的 origin 執行 SVG 裡的 <script>', async () => {
  for (const type of ['image/svg+xml', 'IMAGE/SVG+XML; charset=utf-8', 'image/svg']) {
    await assert.rejects(fetchRemoteImageBlob('https://example.com/a.svg', fakeFetch({ type })), /不支援|不是圖片/, type)
  }
  assert.equal(safeImageType('image/svg+xml'), null)
  assert.equal(safeImageType('text/html'), null)
  assert.equal(safeImageType('image/x-unknown'), null)
})

test('收下的圖片一律用白名單的型別重包成 blob（不照抄對方宣稱的 Content-Type 參數）', async () => {
  const blob = await fetchRemoteImageBlob('https://example.com/a.png', fakeFetch({ type: 'Image/PNG; charset=utf-8' }))
  assert.equal(blob.type, 'image/png')
  assert.equal(safeImageType('image/jpeg'), 'image/jpeg')
  assert.equal(safeImageType('image/webp'), 'image/webp')
})

test('上限在讀完整個回應之前就擋：Content-Length 先看，串流邊讀邊數、超過就 cancel', async () => {
  await assert.rejects(fetchRemoteImageBlob('https://example.com/a', fakeFetch({ length: String(REMOTE_IMAGE_MAX_BYTES + 1) })), /太大/)
  const cancelled = { v: false }
  const half = Math.ceil(REMOTE_IMAGE_MAX_BYTES / 2) + 1
  await assert.rejects(fetchRemoteImageBlob('https://example.com/a', fakeFetch({ chunks: [half, half, 1], cancelled })), /太大/)
  assert.equal(cancelled.v, true, '超過上限要停止讀取')
  const ok = await fetchRemoteImageBlob('https://example.com/a', fakeFetch({ chunks: [3, 4] }))
  assert.equal(ok.size, 7)
})

test('本機／內網位址要標出來（點下去是由這台機器對它發請求）', () => {
  for (const u of ['http://localhost:3000/a.png', 'http://127.0.0.1:7788/api/x', 'http://192.168.1.1/a.png', 'http://10.0.0.5/a.png', 'http://172.20.1.1/a.png', 'http://169.254.169.254/latest/meta-data', 'http://[::1]/a.png', 'http://printer.local/a.png', 'http://[fe80::1]/a.png', 'http://[fd00::1]/a.png', 'http://0.0.0.0/a']) {
    assert.equal(remoteImageInfo(u)?.internal, true, u)
  }
  for (const u of ['https://example.com/a.png', 'https://img.shields.io/x', 'http://172.32.0.1/a.png', 'http://8.8.8.8/a.png']) {
    assert.equal(remoteImageInfo(u)?.internal, false, u)
  }
})
