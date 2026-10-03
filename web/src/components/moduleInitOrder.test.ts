import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'

// 正式 build 把 `src/components/` 拆成 initial-ui chunk，它跟 main chunk（store、api/types）互相 import；
// bundle 之後 main 的 `var X = …` 還沒執行，initial-ui 的模組層就已經在跑。所以 components 的**模組層**
// 不能呼叫從 api／store／lib 來的函式：dev（未打包的 ESM）永遠看不出來，正式站卻整頁空白
// （TypeError: Cannot read properties of undefined (reading 'claude:opus')，2026-10-03）。
// 需要的值請在函式／hook 裡再算。

const dir = new URL('./', import.meta.url)
const files = readdirSync(dir).filter((f) => /\.tsx?$/.test(f) && !/\.test\.tsx?$/.test(f))

/** 從 '../api|store|lib/…' 匯入的名稱（只看值，不看 `import type`）。 */
function crossChunkNames(src: string): Set<string> {
  const names = new Set<string>()
  for (const m of src.matchAll(/^import\s+(?!type\b)\{([^}]*)\}\s+from\s+'\.\.\/(?:api|store|lib)\/[^']*'/gm)) {
    for (const part of m[1]!.split(',')) {
      const name = part.trim().replace(/^type\s.*/, '').split(/\s+as\s+/).pop()!.trim()
      if (name) names.add(name)
    }
  }
  return names
}

test('components 的模組層不呼叫 api／store／lib 的函式（chunk 初始化順序）', () => {
  const offenders: string[] = []
  for (const f of files) {
    const src = readFileSync(new URL(f, dir), 'utf8')
    const names = crossChunkNames(src)
    if (names.size === 0) continue
    // 模組層＝第 0 欄開頭的敘述；函式／類別內文都有縮排。
    for (const m of src.matchAll(/^(?:export\s+)?(?:const|let|var)\s+[\w$]+(?:\s*:[^=\n]+)?\s*=\s*([\w$]+)\(/gm)) {
      if (names.has(m[1]!)) offenders.push(`${f}: ${m[0].trim()}`)
    }
  }
  assert.deepEqual(offenders, [])
})

test('守衛本身抓得到它要抓的寫法', () => {
  const src = "import { canonicalModel } from '../api/types'\nconst OPUS = canonicalModel('claude', 'opus')\n"
  assert.deepEqual([...crossChunkNames(src)], ['canonicalModel'])
  assert.match(src, /^(?:export\s+)?(?:const|let|var)\s+[\w$]+(?:\s*:[^=\n]+)?\s*=\s*canonicalModel\(/m)
})
