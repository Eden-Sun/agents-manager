import test from 'node:test'
import assert from 'node:assert/strict'
import { REMOTE_IMAGE_MAX_BYTES, fetchRemoteImageBlob, isRemoteHttpImage, remoteImageInfo } from './remoteImage.ts'

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

const fakeFetch = (res: { ok?: boolean; status?: number; type?: string; size?: number }, seen: { init?: RequestInit }[] = []) =>
  (async (_url: string, init?: RequestInit) => {
    seen.push({ init })
    return {
      ok: res.ok ?? true,
      status: res.status ?? 200,
      headers: { get: (k: string) => (k.toLowerCase() === 'content-type' ? (res.type ?? 'image/png') : null) },
      blob: async () => new Blob([new Uint8Array(res.size ?? 4)]),
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
