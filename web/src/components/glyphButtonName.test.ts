import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'

const PENDING = new Set(['Sidebar.tsx'])

test('內容只有符號的 <button> 都帶 aria-label', () => {
  const dir = new URL('.', import.meta.url).pathname
  const bad: string[] = []
  for (const f of readdirSync(dir).filter((x) => x.endsWith('.tsx') && !PENDING.has(x))) {
    const s = readFileSync(join(dir, f), 'utf8')
    for (const m of s.matchAll(/>\s*([^\w\s<>{}\u4e00-\u9fff]{1,3})\s*<\/button>/gu)) {
      const start = s.lastIndexOf('<button', m.index)
      if (start < 0 || m.index! - start > 600) continue
      if (!s.slice(start, m.index).includes('aria-label')) bad.push(`${f}:${s.slice(0, m.index).split('\n').length} ${m[1]}`)
    }
  }
  assert.deepEqual(bad, [])
})
