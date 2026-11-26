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
