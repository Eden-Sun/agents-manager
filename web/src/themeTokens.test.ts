import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'

const root = new URL('.', import.meta.url)
const all = readdirSync(root, { recursive: true }).map(String)
const read = (f: string) => readFileSync(new URL(f, root), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '')
const cssFiles = all.filter((f) => f.endsWith('.css'))
const scripts = all.filter((f) => /\.tsx?$/.test(f) && !/\.test\.tsx?$/.test(f))

test('web/src 的 CSS 只用有定義的自訂屬性（CSS 裡宣告，或由程式在執行時設定）', () => {
  const defined = new Set<string>()
  for (const f of cssFiles) for (const [, name] of read(f).matchAll(/(--[\w-]+)\s*:/g)) defined.add(name)
  for (const f of scripts) for (const [, name] of read(f).matchAll(/['"`](--[\w-]+)['"`]/g)) defined.add(name)
  const missing: string[] = []
  for (const f of cssFiles) {
    for (const [, name] of read(f).matchAll(/var\((--[\w-]+)/g)) if (!defined.has(name)) missing.push(`${f}: ${name}`)
  }
  assert.deepEqual(missing, [], '用了沒定義的 token：沒有後備值時整條宣告失效，有後備值時主題切換不會跟著變')
})

test('API bot 相關 CSS 不寫死色碼（顏色一律走主題 token，深色才跟得上）', () => {
  const files = ['components/apiBotUsageBar.css', 'components/translateRetro.css', 'components/apiTurnTimeline.css', 'components/apiBotCostOverview.css']
  const literal = /#[0-9a-fA-F]{3,8}\b|\brgba?\(|\bhsla?\(/
  const hits: string[] = []
  for (const f of files) {
    for (const [i, line] of read(f).split('\n').entries()) if (literal.test(line)) hits.push(`${f}:${i + 1}: ${line.trim()}`)
  }
  assert.deepEqual(hits, [], '寫死的色碼（含後備值）：改用 styles.css 的 token；沒有對應的就先在三組主題（亮、系統深、data-theme 深）各加一個')
})
