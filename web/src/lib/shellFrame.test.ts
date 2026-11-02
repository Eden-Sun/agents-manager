import test from 'node:test'
import assert from 'node:assert/strict'
import { cursorSeq, planFrame, splitFrame } from './shellFrame'

test('splitFrame：尾端的換行不算一列，有給 rows 就補空列到那個高度', () => {
  assert.deepEqual(splitFrame('a\nb\n', null), ['a', 'b'])
  assert.deepEqual(splitFrame('a', 3), ['a', '', ''])
  assert.deepEqual(splitFrame('1\n2\n3\n4', 2), ['3', '4'])
  assert.deepEqual(splitFrame('', 2), ['', ''])
})

test('畫面沒變 → 不寫任何東西', () => {
  assert.equal(planFrame(['a', 'b'], ['a', 'b']).out, '')
})

test('只有一列變了 → 只重畫那一列（清那一列再寫，不整屏清）', () => {
  const p = planFrame(['a', 'b', 'c'], ['a', 'X', 'c'])
  assert.equal(p.out, '\x1b[2;1H\x1b[2KX\x1b[0m')
  assert.deepEqual(p.lines, ['a', 'X', 'c'])
})

test('整張往上捲一列 → 先在底列換行把頂上那列推進捲動歷史，再只重畫新的底列', () => {
  const prev = ['x', '$ ls', 'a.txt', '$']
  const next = ['$ ls', 'a.txt', '$', 'new']
  const p = planFrame(prev, next)
  assert.equal(p.out, '\x1b[4;1H\r\n' + '\x1b[4;1H\x1b[2Knew\x1b[0m')
  assert.deepEqual(p.lines, next)
})

test('空白列不算內容相符：兩張空白畫面不會被誤判成捲動', () => {
  const p = planFrame(['', '', ''], ['', '', 'x'])
  assert.equal(p.out, '\x1b[3;1H\x1b[2Kx\x1b[0m')
})

test('列數改了（resize）→ 整屏重畫', () => {
  const p = planFrame(['a', 'b'], ['a', 'b', 'c'])
  assert.ok(p.out.startsWith('\x1b[H\x1b[2J'))
  assert.deepEqual(p.lines, ['a', 'b', 'c'])
})

test('ANSI 的 SGR 在比對時照字串原樣比，顏色不同就算不同列', () => {
  const p = planFrame(['\x1b[31mred\x1b[0m'], ['red'])
  assert.equal(p.out, '\x1b[1;1H\x1b[2Kred\x1b[0m')
})

test('游標擺在最後一個有字的列的字尾（全形字算 2 格），全空白擺左上', () => {
  assert.equal(cursorSeq(['a', '$ echo', '']), '\x1b[2;7H')
  assert.equal(cursorSeq(['a', '\x1b[32m$ x\x1b[0m', '']), '\x1b[2;4H')
  assert.equal(cursorSeq(['你好', '']), '\x1b[1;5H')
  assert.equal(cursorSeq(['', '']), '\x1b[1;1H')
})
