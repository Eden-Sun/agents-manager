import test from 'node:test'
import assert from 'node:assert/strict'
import { compressible, fitSize, hasTransparency, keepCompressed, MAX_EDGE, outputMime, renameFor } from './imageCompress.ts'

test('長邊縮到 1568、等比；本來就在上限內不動', () => {
  assert.equal(MAX_EDGE, 1568)
  assert.deepEqual(fitSize(4032, 3024), { width: 1568, height: 1176 }, '手機橫拍')
  assert.deepEqual(fitSize(3024, 4032), { width: 1176, height: 1568 }, '手機直拍（EXIF 轉正之後）')
  assert.deepEqual(fitSize(1569, 1000), { width: 1568, height: 999 }, '超過 1px 也縮')
  assert.deepEqual(fitSize(2000, 1000), { width: 1568, height: 784 })
  assert.deepEqual(fitSize(1568, 900), { width: 1568, height: 900 }, '剛好在上限')
  assert.deepEqual(fitSize(1200, 800), { width: 1200, height: 800 })
  assert.deepEqual(fitSize(20000, 10), { width: 1568, height: 1 }, '極扁的不縮成 0')
})

test('只壓 JPEG／PNG／WebP：GIF 動畫、SVG、HEIC、非圖片原樣', () => {
  for (const m of ['image/jpeg', 'image/png', 'image/webp']) assert.equal(compressible(m), true, m)
  for (const m of ['image/gif', 'image/svg+xml', 'image/heic', 'application/pdf', 'text/csv', '']) assert.equal(compressible(m), false, m)
})

test('格式：JPEG 出 JPEG、PNG 維持 PNG（截圖文字不糊）、WebP 看透明度', () => {
  assert.equal(outputMime('image/jpeg', false), 'image/jpeg')
  assert.equal(outputMime('image/png', false), 'image/png', '不透明的 PNG 截圖也不轉 JPEG')
  assert.equal(outputMime('image/png', true), 'image/png')
  assert.equal(outputMime('image/webp', false), 'image/jpeg')
  assert.equal(outputMime('image/webp', true), 'image/png')
})

test('透明度偵測只看 alpha 通道', () => {
  assert.equal(hasTransparency([10, 20, 30, 255, 0, 0, 0, 255]), false)
  assert.equal(hasTransparency([255, 255, 255, 255, 0, 0, 0, 254]), true)
  assert.equal(hasTransparency([0, 0, 0, 0]), true)
})

test('換格式才換副檔名', () => {
  assert.equal(renameFor('IMG_6619.jpeg', 'image/jpeg'), 'IMG_6619.jpeg')
  assert.equal(renameFor('a.JPG', 'image/jpeg'), 'a.JPG')
  assert.equal(renameFor('shot.png', 'image/png'), 'shot.png')
  assert.equal(renameFor('pic.webp', 'image/jpeg'), 'pic.jpg')
  assert.equal(renameFor('pic.webp', 'image/png'), 'pic.png')
  assert.equal(renameFor('noext', 'image/jpeg'), 'noext.jpg')
  assert.equal(renameFor('.webp', 'image/jpeg'), 'image.jpg')
})

test('壓出來沒有比原檔小就用原檔', () => {
  assert.equal(keepCompressed(2_600_000, 400_000), true)
  assert.equal(keepCompressed(50_000, 50_000), false, '一樣大')
  assert.equal(keepCompressed(50_000, 61_000), false, '變大')
  assert.equal(keepCompressed(50_000, 0), false, '編碼失敗')
})
