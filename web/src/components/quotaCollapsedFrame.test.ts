import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./quotaStrip.css', import.meta.url), 'utf8')

/** 取出「選擇器 {…}」那條規則的內容（只認行首的選擇器，避免 `.main-head .quota-hp` 被當成 `.quota-hp`）。 */
function ruleBody(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const m = css.match(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'm'))
  assert.ok(m, `quotaStrip.css 要有 ${selector} 規則`)
  return m[1]
}

/**
 * #1238 之後的 #1268：收合（寬度不夠放完整量表）時，每格框要貼著內容。
 * 格子的 `flex-grow` 會把標題列剩下的寬度分給每一格，框內就多出一段空白（留白在數字右邊）；
 * 圖示欄 `3.2em` 固定寬，左邊也比右邊多一截。收合格改成內容寬，多出來的寬度留在格子外面。
 */
test('收合的格子不吃標題列剩下的寬度：flex 不 grow、也不寫死寬度（框貼著內容）', () => {
  const body = ruleBody('.main-head .quota-open.collapsed .quota-hp')
  assert.match(body, /flex:\s*0 0 auto;/, '收合的格子要 flex: 0 0 auto，不能 grow')
  assert.doesNotMatch(body, /\bwidth:/, '收合的格子不寫死寬度')
})

test('收合的額度列靠左貼著標題，剩下的寬度留在最右（不在框裡、也不在標題與額度之間，#1269）', () => {
  const body = ruleBody('.main-head .quota-open.collapsed')
  assert.match(body, /justify-content:\s*flex-start;/)
})

test('收合的圖示欄跟內容等寬：左內距只剩格子自己的 7px（不再有 3.2em 的空欄）', () => {
  const body = ruleBody('.main-head .quota-open.collapsed .quota-head')
  assert.match(body, /width:\s*auto;/)
  assert.match(body, /padding-right:\s*8px;/, '右邊留 8px 給窗口名，不貼著 cc0')
})
