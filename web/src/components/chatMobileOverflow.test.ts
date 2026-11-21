import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./chatPanel.css', import.meta.url), 'utf8')
const styles = await readFile(new URL('../styles.css', import.meta.url), 'utf8')

function ruleBody(src: string, selector: string): string {
  const i = src.indexOf(`${selector} {`)
  assert.ok(i >= 0, `找不到規則 ${selector}`)
  return src.slice(i, src.indexOf('}', i))
}

test('譯文狀態列所在的 meta 列：有狀態列時可換行、高度跟內容走', () => {
  const body = ruleBody(css, '.msg-meta:has(.msg-translate-hint)')
  assert.match(body, /flex-wrap:\s*wrap;/)
  assert.match(body, /white-space:\s*normal;/)
  assert.match(body, /height:\s*auto;/)
})

test('meta 列與其左側子項可縮（min-width:0），不再撐寬泡泡', () => {
  assert.match(ruleBody(css, '.msg-meta-left'), /min-width:\s*0;/)
  assert.match(css, /\.msg-meta-above,\s*\.msg-meta-below,\s*\.msg-meta-left\s*\{\s*min-width:\s*0;\s*max-width:\s*100%;/)
})

test('譯文狀態列自己會斷行（長字串 overflow-wrap:anywhere）', () => {
  const body = ruleBody(css, '.msg-meta .msg-translate-hint')
  assert.match(body, /min-width:\s*0;/)
  assert.match(body, /overflow-wrap:\s*anywhere;/)
})

test('表格與 code block 在自己內部橫向捲動，不靠整體 overflow-x', () => {
  assert.match(ruleBody(styles, '.bubble.md table'), /overflow-x:\s*auto;/)
  assert.match(ruleBody(styles, '.bubble.md pre'), /overflow(-x)?:\s*auto;/)
})

test('OpenAI 回合的 token 拆解列：有拆解時可換行、拆解本身可縮並斷行', () => {
  const meta = ruleBody(css, '.msg-meta:has(.api-openai-breakdown)')
  assert.match(meta, /flex-wrap:\s*wrap;/)
  assert.match(meta, /white-space:\s*normal;/)
  assert.match(meta, /height:\s*auto;/)
  const item = ruleBody(css, '.msg-meta .api-openai-breakdown')
  assert.match(item, /min-width:\s*0;/)
  assert.match(item, /max-width:\s*100%;/)
  assert.match(item, /overflow-wrap:\s*anywhere;/)
})

test('助理 markdown 泡泡守在欄寬內（min-width:0；max-width:100%）', () => {
  const body = ruleBody(styles, '.bubble.md')
  assert.match(body, /min-width:\s*0;/)
  assert.match(body, /max-width:\s*100%;/)
})

test('行內 code 可在任意字元斷行（長路徑不撐寬）', () => {
  assert.match(ruleBody(styles, '.bubble.md code'), /overflow-wrap:\s*anywhere;/)
})

test('訊息列表不產生水平捲動（保險：overflow-x:hidden；根因已在各子元素修）', () => {
  const body = ruleBody(styles, '.msg-list')
  assert.match(body, /overflow-y:\s*auto;/)
  assert.match(body, /overflow-x:\s*hidden;/)
})
