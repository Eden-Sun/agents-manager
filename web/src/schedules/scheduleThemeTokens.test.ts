import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'

const files = readdirSync(new URL('.', import.meta.url)).filter((f) => f.endsWith('.css'))
const theme = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')

test('排程 CSS 不寫死顏色，也不用不存在的 --surface token（深色主題才跟著變）', () => {
  assert.ok(files.length >= 4)
  for (const f of files) {
    const css = readFileSync(new URL(f, import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '')
    assert.doesNotMatch(css, /#[0-9a-fA-F]{3,8}\b/, `${f} 有寫死的色碼`)
    assert.doesNotMatch(css, /var\(--surface/, `${f} 用了不存在的 --surface`)
    assert.doesNotMatch(css, /\b(?:background|color)\s*:\s*(?:white|black)\b/i, `${f} 有寫死的白／黑`)
  }
})

test('Runner Bot 選項卡片明寫文字色，不吃按鈕的瀏覽器預設黑字', () => {
  const css = readFileSync(new URL('scheduleEditor.css', import.meta.url), 'utf8')
  const row = /\.sched-bot-row\s*\{([^}]*)\}/.exec(css)?.[1] ?? ''
  assert.match(row, /background:\s*var\(--bg-panel\)/)
  assert.match(row, /color:\s*var\(--text\)/)
})

test('排程 CSS 用到的 token 都在主題裡定義', () => {
  for (const f of files) {
    const css = readFileSync(new URL(f, import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '')
    for (const [, name] of css.matchAll(/var\((--[\w-]+)/g)) assert.match(theme, new RegExp(`${name}\\s*:`), `${f} 用的 ${name} 沒有在 styles.css 定義`)
  }
})
