import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

/**
 * issue #548：寫 `--vvh` 的那一側（`useViewportPin`）與吃它的那一側（`styles.css`）必須是同一個門檻。
 * 只放寬一邊的話另一邊會靜靜失效——原本就是 hook 走 ≤640、CSS 也走 ≤640，iPad 直式（768／820）兩邊都沒有，
 * 鍵盤一彈出來就把底部輸入列蓋住。這一條只認得出「兩邊不一致」，實機行為沒辦法在這裡驗。
 */
test('--vvh 的寫入端與使用端都掛在抽屜版面（≤1024px）', () => {
  const hook = readFileSync(new URL('./useViewportPin.ts', import.meta.url), 'utf8')
  assert.match(hook, /useMediaQuery\(DRAWER_QUERY\)/)
  assert.doesNotMatch(hook, /useMediaQuery\(PHONE_QUERY\)/, '註解可以提到它，但不能再用它當門檻')

  const css = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')
  const at = css.indexOf('height: var(--vvh, 100dvh);')
  assert.ok(at > 0, 'styles.css 要有 html/body/#root 的 height: var(--vvh, 100dvh)')
  // 包住那一行的是哪一個 @media：往前找最近的那個門檻，它要涵蓋抽屜版面。
  const before = css.slice(0, at).match(/@media \(width <= (\d+)px\)(?![\s\S]*@media \(width <= \d+px\))/)
  assert.ok(before, '那一行要在某個 max-width 的 @media 裡')
  assert.equal(Number(before![1]), 1024, `--vvh 的高度規則掛在 ≤${before![1]}px，iPad 直式（768／820）吃不到`)
})
