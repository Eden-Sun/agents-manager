import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'

// 只有一個符號（✕ ↑ ⌂ ✎ ← →）的按鈕，可及名稱來自內容而不是 title，讀屏念的是符號本身。要有 aria-label。
// Sidebar.tsx 的搜尋清除鈕（✕）先放行：那支檔案有人在動，等他推完再補。
const PENDING = new Set(['Sidebar.tsx'])

test('內容只有符號的 <button> 都帶 aria-label', () => {
  const dir = new URL('.', import.meta.url).pathname
  const bad: string[] = []
  for (const f of readdirSync(dir).filter((x) => x.endsWith('.tsx') && !PENDING.has(x))) {
    const s = readFileSync(join(dir, f), 'utf8')
    // 從「內容只有符號」往回找最近的 <button：屬性裡的 `used > 0` 之類會讓「找到第一個 >」的寫法斷掉。
    for (const m of s.matchAll(/>\s*([^\w\s<>{}\u4e00-\u9fff]{1,3})\s*<\/button>/gu)) {
      const start = s.lastIndexOf('<button', m.index)
      if (start < 0 || m.index! - start > 600) continue
      if (!s.slice(start, m.index).includes('aria-label')) bad.push(`${f}:${s.slice(0, m.index).split('\n').length} ${m[1]}`)
    }
  }
  assert.deepEqual(bad, [])
})
