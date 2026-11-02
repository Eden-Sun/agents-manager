import test from 'node:test'
import assert from 'node:assert/strict'
import { fontSizeFor, MIN_READABLE_FONT } from './shellFont'

test('預設：pane 放不下也不縮到 12px 以下，剩下交給容器捲動', () => {
  assert.equal(fontSizeFor({ width: 700, height: 250 }, { cols: 213, rows: 55 }, false), MIN_READABLE_FONT)
})

test('預設：盒子夠大就用到 13px 上限', () => {
  assert.equal(fontSizeFor({ width: 2000, height: 2000 }, { cols: 80, rows: 24 }, false), 13)
})

test('縮放至全螢幕：縮到整個 pane 放得下（不低於 3px）', () => {
  const size = fontSizeFor({ width: 1000, height: 800 }, { cols: 213, rows: 55 }, true)
  assert.ok(size >= 3 && size < 12, `size=${size}`)
  assert.ok(213 * 0.6 * size <= 1000 && 55 * 1.2 * size <= 800)
})
