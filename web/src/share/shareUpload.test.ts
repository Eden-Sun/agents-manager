/**
 * 分享頁上傳（SPEC §20.4）：照片轉 JPEG／縮小、429 照 Retry-After 自己重試。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import type { ShareClient } from './shareApi'
import { ShareHttpError } from './shareModel'
import { PhotoUnreadableError, PHOTO_MAX_EDGE, preparePhoto, uploadErrorText, uploadPatiently, uploadRetryable, type ImageCodec } from './shareUpload'

/** 假的解碼／編碼：記下畫成多大，回固定大小的 JPEG。 */
function fakeCodec(width: number, height: number, opts: { decodable?: boolean; outBytes?: number } = {}) {
  const drawn: Array<[number, number]> = []
  const codec: ImageCodec = {
    async decode() {
      return opts.decodable === false ? null : { width, height, source: {} as CanvasImageSource, close() {} }
    },
    async encodeJpeg(_s, w, h) {
      drawn.push([w, h])
      return new Blob([new Uint8Array(opts.outBytes ?? 1000)], { type: 'image/jpeg' })
    },
  }
  return { codec, drawn }
}

const file = (name: string, type: string, bytes = 5_000_000) => new File([new Uint8Array(bytes)], name, { type })

test('iPhone 的 HEIC 轉成 JPEG（bot 的 Read 讀不了 HEIC），長邊縮到 2048', async () => {
  const { codec, drawn } = fakeCodec(4032, 3024)
  const out = await preparePhoto(file('IMG_0001.HEIC', 'image/heic'), codec)
  assert.equal(out.name, 'IMG_0001.jpg')
  assert.equal(out.type, 'image/jpeg')
  assert.deepEqual(drawn, [[PHOTO_MAX_EDGE, 1536]])
})

test('小張的 HEIC 也要轉，但不放大', async () => {
  const { codec, drawn } = fakeCodec(800, 600)
  const out = await preparePhoto(file('a.heif', ''), codec)
  assert.equal(out.name, 'a.jpg')
  assert.deepEqual(drawn, [[800, 600]])
})

test('瀏覽器解不開的 HEIC：說「這張照片打不開」，不送一個 bot 讀不了的檔', async () => {
  const { codec } = fakeCodec(1, 1, { decodable: false })
  await assert.rejects(preparePhoto(file('x.heic', 'image/heic'), codec), PhotoUnreadableError)
  assert.match(uploadErrorText(new PhotoUnreadableError()), /打不開/)
  assert.equal(uploadRetryable(new PhotoUnreadableError()), false)
})

test('大張 JPEG 縮小；本來就不大的 JPEG、PNG 截圖原樣送', async () => {
  const big = fakeCodec(3000, 4000)
  const out = await preparePhoto(file('photo.jpeg', 'image/jpeg'), big.codec)
  assert.deepEqual(big.drawn, [[1536, PHOTO_MAX_EDGE]])
  assert.equal(out.name, 'photo.jpg')
  assert.ok(out.size < 5_000_000)

  const small = fakeCodec(1200, 900)
  const orig = file('s.jpg', 'image/jpeg')
  assert.equal(await preparePhoto(orig, small.codec), orig)
  assert.deepEqual(small.drawn, [])

  const png = file('shot.png', 'image/png')
  const never = fakeCodec(5000, 5000)
  assert.equal(await preparePhoto(png, never.codec), png)
  assert.deepEqual(never.drawn, [], '截圖不轉 JPEG：字會糊')
})

function clientWith(results: Array<Error | null>): { client: ShareClient; calls: () => number } {
  let n = 0
  const client = {
    async upload(f: File) {
      const r = results[n++]
      if (r) throw r
      return { id: `id-${n}`, name: f.name }
    },
  } as unknown as ShareClient
  return { client, calls: () => n }
}

test('429 照 Retry-After 等了自己重試，不把「太快」丟給使用者', async () => {
  const slept: number[] = []
  const { client, calls } = clientWith([new ShareHttpError(429, 2), new ShareHttpError(429, null), null])
  const r = await uploadPatiently(client, file('a.txt', 'text/plain', 3), { sleep: async (ms) => void slept.push(ms) })
  assert.equal(r.id, 'id-3')
  assert.equal(calls(), 3)
  assert.deepEqual(slept, [2000, 3000], 'Retry-After 沒給時等 3 秒')
})

test('Retry-After 太長也只等 30 秒就再試；一直 429 終究會放棄', async () => {
  const slept: number[] = []
  const { client, calls } = clientWith(Array.from({ length: 20 }, () => new ShareHttpError(429, 120)))
  await assert.rejects(uploadPatiently(client, file('a.txt', 'text/plain', 3), { sleep: async (ms) => void slept.push(ms) }), ShareHttpError)
  assert.ok(slept.every((ms) => ms === 30_000))
  assert.equal(calls(), slept.length + 1)
  assert.ok(calls() > 5 && calls() < 20)
})

test('其他錯誤不重試；太大的檔不送', async () => {
  const { client, calls } = clientWith([new ShareHttpError(415)])
  await assert.rejects(uploadPatiently(client, file('a.txt', 'text/plain', 3), { sleep: async () => {} }), (e: unknown) => e instanceof ShareHttpError && e.status === 415)
  assert.equal(calls(), 1)
  assert.equal(uploadRetryable(new ShareHttpError(415)), false)
  assert.equal(uploadRetryable(new Error('network')), true)

  const huge = clientWith([null])
  await assert.rejects(uploadPatiently(huge.client, file('big.zip', 'application/zip', 26 * 1024 * 1024)), (e: unknown) => e instanceof ShareHttpError && e.status === 413)
  assert.equal(huge.calls(), 0)
})

test('503（入口暫時忙，Retry-After）照 Retry-After 等了自己重試', async () => {
  const slept: number[] = []
  const { client, calls } = clientWith([new ShareHttpError(503, 5), new ShareHttpError(503, null), null])
  const r = await uploadPatiently(client, file('a.txt', 'text/plain', 3), { sleep: async (ms) => void slept.push(ms) })
  assert.equal(r.id, 'id-3')
  assert.equal(calls(), 3)
  assert.deepEqual(slept, [5000, 3000], 'Retry-After 沒給時等 3 秒')
})

test('一直 503 只試 3 次就放棄，錯誤照拋、仍可「再試一次」', async () => {
  const { client, calls } = clientWith(Array.from({ length: 10 }, () => new ShareHttpError(503, 5)))
  await assert.rejects(uploadPatiently(client, file('a.txt', 'text/plain', 3), { sleep: async () => {} }), (e: unknown) => e instanceof ShareHttpError && e.status === 503)
  assert.equal(calls(), 4)
  assert.equal(uploadRetryable(new ShareHttpError(503)), true)
})

test('429 與 503 混著來：429 不吃 503 的額度', async () => {
  const { client, calls } = clientWith([new ShareHttpError(503, 1), new ShareHttpError(429, 1), new ShareHttpError(429, 1), new ShareHttpError(503, 1), null])
  const r = await uploadPatiently(client, file('a.txt', 'text/plain', 3), { sleep: async () => {} })
  assert.equal(r.id, 'id-5')
  assert.equal(calls(), 5)
})
