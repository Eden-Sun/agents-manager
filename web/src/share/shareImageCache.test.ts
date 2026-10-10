/**
 * 分享頁點陣圖快取（#1175）：同名的舊版本一進新版就淘汰；清單裡已經沒有的檔案整條淘汰；總量有上限；
 * 抓取失敗不留；舊版的抓取還在路上、新版已經來了，舊版回來也不塞回快取。
 * 只看「同一個檔被抓了幾次」，不碰 canvas：SVG 轉圖在這裡失敗，走的是「這一版載不出來」的快取路徑（同樣會留快取）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import * as images from './shareImageCache'
import type { ShareClient } from './shareApi'
import type { ShareFile } from './shareModel'

type Api = {
  pngOf(client: ShareClient, name: string, version: string): Promise<unknown>
  rasterOf(client: ShareClient, name: string, version: string): Promise<Blob>
  pruneShareImages(client: ShareClient, files: ShareFile[]): void
  SHARE_IMAGE_CACHE_LIMITS: { png: { maxEntries: number; maxBytes: number }; raster: { maxEntries: number; maxBytes: number } }
}
const api = images as unknown as Api

/** 每次 fileBlob 都記一筆檔名；`bytes` 決定回傳的「Blob」大小（點陣圖快取只看 size）。 */
function fakeClient(make: (name: string) => Promise<Blob> | Blob = () => new Blob(['not svg'])) {
  const calls: string[] = []
  const client = {
    fileBlob: (name: string) => {
      calls.push(name)
      return Promise.resolve().then(() => make(name))
    },
  } as unknown as ShareClient
  return { client, calls }
}
const count = (calls: string[], name: string) => calls.filter((n) => n === name).length
const file = (name: string): ShareFile => ({ name, size: 1, modified_at: null, version: 'v1' })

test('SVG 同名的舊版本一進新版就淘汰：bot 改了 50 次，最新版還在，回頭要的舊版要重抓', async () => {
  const { client, calls } = fakeClient()
  for (let i = 1; i <= 50; i++) await api.pngOf(client, 'report.svg', `v${i}`)
  assert.equal(calls.length, 50)
  await api.pngOf(client, 'report.svg', 'v50')
  assert.equal(calls.length, 50, '最新版還在快取裡，不重抓')
  await api.pngOf(client, 'report.svg', 'v1')
  assert.equal(calls.length, 51, 'v1 已被同名的新版淘汰，要重抓')
})

test('同一版重複要只抓一次', async () => {
  const { client, calls } = fakeClient()
  await Promise.all([api.pngOf(client, 'a.svg', 'v1'), api.pngOf(client, 'a.svg', 'v1')])
  await api.pngOf(client, 'a.svg', 'v1')
  assert.equal(calls.length, 1)
})

test('抓取失敗不留快取：網路斷了一次，下次要再抓', async () => {
  let n = 0
  const client = {
    fileBlob: async () => {
      n++
      if (n === 1) throw new Error('net')
      return new Blob(['not svg'])
    },
  } as unknown as ShareClient
  assert.equal(await api.pngOf(client, 'a.svg', 'v1'), 'broken', '這一版先顯示「還在修」，不卡住')
  await api.pngOf(client, 'a.svg', 'v1')
  assert.equal(n, 2, '失敗的那次不留，第二次要真的重抓')
})

test('舊版的抓取還在路上、新版已經來了：舊版回來也不會塞回快取，回頭要的時候重抓', async () => {
  let release!: () => void
  const gate = new Promise<void>((r) => (release = r))
  let n = 0
  const client = {
    fileBlob: async () => {
      n++
      if (n === 1) await gate
      return new Blob(['not svg'])
    },
  } as unknown as ShareClient
  const old = api.pngOf(client, 'a.svg', 'v1')
  const cur = api.pngOf(client, 'a.svg', 'v2')
  release()
  await Promise.all([old, cur])
  await api.pngOf(client, 'a.svg', 'v1')
  assert.equal(n, 3, 'v1 已被 v2 淘汰，舊版回來不能讓它又留在快取裡')
})

test('點陣圖：清單裡已經沒有的檔案（刪掉、改名）整條淘汰；還在清單裡的留著', async () => {
  const { client, calls } = fakeClient(() => ({ size: 10 }) as unknown as Blob)
  await api.rasterOf(client, 'a.png', 'v1')
  await api.rasterOf(client, 'b.png', 'v1')
  api.pruneShareImages(client, [file('b.png')])
  await api.rasterOf(client, 'b.png', 'v1')
  assert.equal(count(calls, 'b.png'), 1, '還在清單裡，不重抓')
  await api.rasterOf(client, 'a.png', 'v1')
  assert.equal(count(calls, 'a.png'), 2, '已不在清單，淘汰後要重抓')
})

test('點陣圖總量有上限：一直開新的檔，最舊的會被淘汰，最新的還在', async () => {
  const { maxBytes } = api.SHARE_IMAGE_CACHE_LIMITS.raster
  const chunk = Math.floor(maxBytes / 4)
  const { client, calls } = fakeClient(() => ({ size: chunk }) as unknown as Blob)
  // 共 8 張、是上限的 2 倍：不能只長不縮。
  for (let i = 0; i < 8; i++) await api.rasterOf(client, `f${i}.png`, 'v1')
  await api.rasterOf(client, 'f0.png', 'v1')
  assert.equal(count(calls, 'f0.png'), 2, '最舊的超出上限被淘汰，要重抓')
  await api.rasterOf(client, 'f7.png', 'v1')
  assert.equal(count(calls, 'f7.png'), 1, '最新的還在快取裡')
})
