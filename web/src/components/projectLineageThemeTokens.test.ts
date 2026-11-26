import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const css = readFileSync(new URL('projectLineageSection.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '')
const theme = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')

test('專案血統區塊不寫死顏色，也沒有後備值（淺色主題才跟著變）', () => {
  assert.doesNotMatch(css, /#[0-9a-fA-F]{3,8}\b/, '有寫死的色碼')
  assert.doesNotMatch(css, /\brgba?\(/, '有寫死的 rgb／rgba')
  assert.doesNotMatch(css, /var\(--[\w-]+\s*,/, 'token 帶後備值：token 不存在時會悄悄落到後備色')
})

test('專案血統區塊用到的 token 都在主題裡定義', () => {
  const used = new Set([...css.matchAll(/var\((--[\w-]+)\)/g)].map((m) => m[1]))
  assert.ok(used.size > 0)
  for (const name of used) assert.match(theme, new RegExp(`${name}\\s*:`), `${name} 沒有在 styles.css 定義`)
})
