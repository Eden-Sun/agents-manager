import test from 'node:test'
import assert from 'node:assert/strict'
import { reflowForPhone } from './termReflow.ts'

test('213 欄的外框：框線列拿掉、兩端框邊拿掉、補滿的空白拿掉，字不變', () => {
  const w = 209
  const screen = [
    `╭${'─'.repeat(w + 2)}╮`,
    `│ ${'Do you want to proceed?'.padEnd(w)} │`,
    `│ ${'❯ 1. Yes'.padEnd(w)} │`,
    `│ ${'  2. No, and tell Claude what to do differently'.padEnd(w)} │`,
    `╰${'─'.repeat(w + 2)}╯`,
  ].join('\n')
  assert.equal(reflowForPhone(screen), ['Do you want to proceed?', '❯ 1. Yes', '  2. No, and tell Claude what to do differently'].join('\n'))
})

test('空列：連續的收成一列，頭尾的去掉', () => {
  assert.equal(reflowForPhone('\n\n  \na\n\n\n\nb\n   \n\n'), 'a\n\nb')
})

test('縮排最多留四格，行內一長串對齊用的空白收成兩個；一兩個空白不動', () => {
  assert.equal(reflowForPhone(`${' '.repeat(40)}deep`), '    deep')
  assert.equal(reflowForPhone('  two'), '  two')
  assert.equal(reflowForPhone(`name${' '.repeat(60)}value  x y`), 'name  value  x y')
})

test('只有一邊有直線的是內文（引用、樹狀圖），不拆；表格中間的直線也留著', () => {
  assert.equal(reflowForPhone('│ quoted text'), '│ quoted text')
  assert.equal(reflowForPhone('├── src'), '├── src')
  assert.equal(reflowForPhone('│ a │ b │'), 'a │ b')
})

test('一般文字、分隔線、提示符原樣', () => {
  const plain = ['plain text', '─'.repeat(32), '> ', 'rm -rf x ──── y'].join('\n')
  assert.equal(reflowForPhone(plain), ['plain text', '─'.repeat(32), '>', 'rm -rf x ──── y'].join('\n'))
})
