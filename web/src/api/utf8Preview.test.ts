import test from 'node:test'
import assert from 'node:assert/strict'
import { utf8PreviewUrl } from './index'

const bytes = async (u: string) => new Uint8Array(await (await (await fetch(u)).blob()).arrayBuffer())

test('預覽的文字檔前面補 UTF-8 BOM（Safari 開 blob 不看 charset，中文 md 會亂碼）', async () => {
  const src = URL.createObjectURL(new Blob(['# 資訊需求單'], { type: 'text/plain; charset=utf-8' }))
  const b = await bytes(await utf8PreviewUrl(src))
  assert.deepEqual(Array.from(b.slice(0, 3)), [0xef, 0xbb, 0xbf])
  assert.equal(new TextDecoder().decode(b.slice(3)), '# 資訊需求單')
})

test('非文字檔與已有 BOM 的不動', async () => {
  const png = URL.createObjectURL(new Blob([new Uint8Array([0x89, 0x50])], { type: 'image/png' }))
  assert.equal(await utf8PreviewUrl(png), png)
  const bom = URL.createObjectURL(new Blob([new Uint8Array([0xef, 0xbb, 0xbf, 0x41])], { type: 'text/plain' }))
  assert.equal(await utf8PreviewUrl(bom), bom)
})
