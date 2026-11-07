import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const chipCss = await readFile(new URL('./unreadChip.css', import.meta.url), 'utf8')
const styles = await readFile(new URL('../styles.css', import.meta.url), 'utf8')

const zOf = (css: string, selector: string, from = 0): number => {
  const i = css.indexOf(`${selector} {`, from)
  assert.ok(i >= 0, `找不到 ${selector}`)
  const m = css.slice(i, css.indexOf('}', i)).match(/z-index:\s*(\d+)/)
  assert.ok(m, `${selector} 沒有 z-index`)
  return Number(m[1])
}

/** 2026-11-07：手機主力區的拉鈕（`.pin-rows-handle`）穿透額度浮層。手機 `.main-head` 是 sticky 並自成 stacking context（浮層的 z-index 80 被關在裡面），
 * 拉鈕的 z-index 只要 ≥ header 的，又在 DOM 後面，就會畫在浮層上。 */
test('手機主力區拉鈕的 z-index 低於 sticky 的 .main-head（不會穿透額度浮層）', () => {
  const phone = styles.indexOf('  .main-head {\n    position: sticky;')
  assert.ok(phone > 0, '手機 .main-head 要是 sticky')
  const head = Number(styles.slice(phone, styles.indexOf('}', phone)).match(/z-index:\s*(\d+)/)?.[1])
  assert.ok(head > 0, '手機 .main-head 要有 z-index')
  const handle = zOf(chipCss, '  .pin-rows-handle')
  assert.ok(handle < head, `拉鈕 z-index ${handle} 必須小於 .main-head 的 ${head}`)
  // 仍要高過旁邊晶片（z-index 2）才點得到、看得到。
  assert.ok(handle > 2, '拉鈕仍要高過晶片的 z-index 2')
})
