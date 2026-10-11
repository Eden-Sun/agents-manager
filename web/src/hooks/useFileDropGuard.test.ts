import test from 'node:test'
import assert from 'node:assert/strict'
import { guardFileDrop } from './useFileDropGuard.ts'

// 假事件：只留 guardFileDrop 會碰到的欄位。
const fake = (type: string, types: string[], over: { defaultPrevented?: boolean } = {}) => {
  const dataTransfer = { types, dropEffect: 'copy' as string }
  const ev = {
    type,
    dataTransfer,
    defaultPrevented: over.defaultPrevented ?? false,
    calls: 0,
    preventDefault() {
      this.calls += 1
    },
  }
  return ev
}
// 假事件只留 guardFileDrop 會碰到的欄位，型別在這裡補齊。
const run = (ev: ReturnType<typeof fake>) => guardFileDrop(ev as unknown as Parameters<typeof guardFileDrop>[0])

test('沒人接的檔案拖放被擋下：dragover 改成 none、drop 也擋（不讓瀏覽器在這個分頁打開檔案）', () => {
  const over = fake('dragover', ['Files'])
  run(over)
  assert.equal(over.calls, 1)
  assert.equal(over.dataTransfer.dropEffect, 'none')

  const drop = fake('drop', ['Files'])
  run(drop)
  assert.equal(drop.calls, 1)
  assert.equal(drop.dataTransfer.dropEffect, 'copy', 'drop 不需要改 dropEffect')
})

test('已經有 drop 區接手（defaultPrevented）的不動', () => {
  const ev = fake('dragover', ['Files'], { defaultPrevented: true })
  run(ev)
  assert.equal(ev.calls, 0)
  assert.equal(ev.dataTransfer.dropEffect, 'copy')
})

test('不是檔案的拖曳不動（側欄拖曳排序、文字、連結）', () => {
  for (const type of ['dragover', 'drop']) {
    const ev = fake(type, ['text/plain'])
    run(ev)
    assert.equal(ev.calls, 0, type)
    assert.equal(ev.dataTransfer.dropEffect, 'copy', type)
  }
})
