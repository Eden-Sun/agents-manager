/**
 * 環境設定頁的手機版樣式（issue #1466）：`envPanelPhone.css` 的規則全部關在 `@media (width <= 640px)` 裡，
 * 桌機（>1024px）與平板（641–1024px）版面不動；手機的觸控目標 ≥44px、字級 ≥13px、沒有巢狀捲動。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const read = (name: string) => readFileSync(new URL(`./${name}`, import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '')

/** 取出 `@media (width <= 640px) {…}` 區塊的內文（括號配對）。回傳 [區塊內文, 區塊外的 css]。 */
function phoneBlocks(css: string): { inside: string; outside: string } {
  const re = /@media \(width <= 640px\)\s*\{/g
  let inside = ''
  let outside = ''
  let last = 0
  for (let m = re.exec(css); m; m = re.exec(css)) {
    let depth = 1
    let i = m.index + m[0].length
    while (i < css.length && depth > 0) {
      if (css[i] === '{') depth++
      else if (css[i] === '}') depth--
      i++
    }
    inside += css.slice(m.index + m[0].length, i - 1)
    outside += css.slice(last, m.index)
    last = i
    re.lastIndex = i
  }
  return { inside, outside: outside + css.slice(last) }
}

/** 選擇器清單裡有 `selector` 的所有規則，宣告區串起來（同一個選擇器常分散在字級、觸控目標等幾條規則）。 */
function decl(css: string, selector: string): string {
  const esc = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const re = new RegExp(`(?:^|[},\\s])${esc}(?![\\w-])[^{}]*\\{([^}]*)\\}`, 'g')
  const all = [...css.matchAll(re)].map((m) => m[1]!)
  assert.ok(all.length > 0, `找不到規則 ${selector}`)
  return all.join('\n')
}

const phone = phoneBlocks(read('envPanelPhone.css'))

test('envPanelPhone.css 只有 ≤640px 的規則（桌機版面不動）', () => {
  assert.equal(phone.outside.trim(), '', `區塊外不該有規則：${phone.outside.trim().slice(0, 80)}`)
  assert.ok(phone.inside.includes('.env-panel'))
})

test('手機：可點的目標 ≥44px', () => {
  for (const sel of ['.env-panel .mini-btn', '.env-panel .btn', '.env-panel .env-sec-toggle', '.env-panel .identity-login-btn', '.env-panel .disclosure.sub', '.env-panel .deleted-restore']) {
    assert.match(decl(phone.inside, sel), /min-height:\s*44px/, `${sel} 要 min-height 44px`)
  }
})

test('手機：文字 ≥13px、輸入框不另設小字級', () => {
  const sizes = [...phone.inside.matchAll(/font-size:\s*([\d.]+)px/g)].map((m) => Number(m[1]))
  assert.ok(sizes.length > 0)
  assert.ok(sizes.every((s) => s >= 13), `env 面板手機字級不得小於 13px：${sizes.join(',')}`)
})

test('手機：清單不自己再捲（不留巢狀捲動陷阱）', () => {
  for (const sel of ['.env-panel .deleted-list', '.env-panel .host-baseline-items']) {
    const d = decl(phone.inside, sel)
    assert.match(d, /max-height:\s*none/)
    assert.match(d, /overflow(-y)?:\s*visible/)
  }
})

test('手機：帳本表格攤成卡片，數字欄不折行；捲動容器仍在', () => {
  const ledger = phoneBlocks(read('judgePanel.css'))
  assert.match(ledger.inside, /\.judge-ledger-table td::before\s*\{[^}]*attr\(data-label\)/)
  assert.match(decl(ledger.outside, '.judge-ledger-table td.num'), /white-space:\s*nowrap/)
  assert.doesNotMatch(ledger.inside, /td\.num\s*\{[^}]*white-space:\s*normal/, '手機也不讓數字欄折行')
  // 桌機規則（外層捲動容器）沒被拿掉。
  assert.match(decl(ledger.outside, '.judge-ledger-scroll'), /overflow-x:\s*auto/)
})
