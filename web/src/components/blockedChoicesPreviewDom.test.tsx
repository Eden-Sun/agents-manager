import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { BlockedChoices } from './BlockedChoices.tsx'
import { parseChoiceMenu } from '../lib/tuiChoices.ts'

const FIXTURE = readFileSync(new URL('../lib/__fixtures__/claude-askq-preview-213x55.txt', import.meta.url), 'utf8')

test('並排預覽版面：選項照舊可點，游標那一項的預覽用等寬字畫在選項下方 (issue #1473)', () => {
  const menu = parseChoiceMenu(FIXTURE)
  assert.ok(menu)
  const html = renderToStaticMarkup(<BlockedChoices botId="b1" menu={menu} />)
  assert.equal((html.match(/class="bc-item/g) ?? []).length, 2)
  assert.match(html, /改回 200 正常訊息 \(Recommended\)/)
  assert.match(html, /<pre class="bc-context bc-notes bc-preview-text">HTTP 200\n\{/)
  assert.match(html, /&quot;status&quot;: &quot;insurer_not_supported&quot;/)
  assert.match(html, /第 1 項的預覽/)
  // 預覽在清單之後。
  assert.ok(html.indexOf('bc-preview-text') > html.lastIndexOf('bc-item'))
  assert.doesNotMatch(html, /[│┌└]/)
})

test('沒有預覽框的選單不畫預覽區', () => {
  const menu = parseChoiceMenu(['問題？', '', '❯ 1. 甲', '  2. 乙', '', 'Enter to select · ↑/↓ to navigate · Esc to cancel'].join('\n'))
  assert.ok(menu)
  const html = renderToStaticMarkup(<BlockedChoices botId="b1" menu={menu} />)
  assert.doesNotMatch(html, /bc-preview/)
})
