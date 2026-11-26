import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const css = readFileSync(new URL('./styles.css', import.meta.url), 'utf8')

test('Markdown 載入前的純文字 fallback 在 .bubble.md 內仍會折行，不被 pre 的 white-space: pre 蓋掉', () => {
  const m = css.match(/\.bubble \.md-plain,\s*\.bubble\.md pre\.md-plain\s*\{([^}]*)\}/)
  assert.ok(m, '.bubble.md pre.md-plain 要有比 .bubble.md pre 更高的 specificity')
  assert.match(m[1], /white-space:\s*pre-wrap/)
  assert.match(m[1], /overflow-wrap:\s*anywhere/)
  assert.match(m[1], /background:\s*transparent/)
})

test('氣泡內 pre 與表格限制在氣泡寬度內並可水平捲動', () => {
  assert.match(css, /\.bubble\.md pre\s*\{[^}]*max-width:\s*100%[^}]*overflow-x:\s*auto/)
  assert.match(css, /\.bubble\.md table\s*\{[^}]*display:\s*block[^}]*max-width:\s*100%[^}]*overflow-x:\s*auto/)
  assert.match(css, /\.bubble\.md p,\s*\.bubble\.md li,\s*\.bubble\.md a\s*\{\s*overflow-wrap:\s*anywhere/)
})

test('平板 toast 放在 header、chip 列與 git 列下方；modal 開啟時讓到左下角', () => {
  assert.match(css, /@media \(641px <= width <= 1366px\)\s*\{\s*\.notices\s*\{\s*top:\s*calc\(env\(safe-area-inset-top\)\s*\+\s*136px\)/)
  assert.match(css, /@media \(width >= 641px\)\s*\{\s*body:has\(\.modal-backdrop, \.confirm-backdrop\) \.notices\s*\{[^}]*top:\s*auto[^}]*right:\s*auto[^}]*bottom:[^}]*left:/)
  assert.match(css, /\.notice\.leaving\s*\{[^}]*animation:\s*notice-out/)
})
