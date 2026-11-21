import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'

const dir = new URL('./', import.meta.url)
const files = readdirSync(dir).filter((f) => /\.tsx?$/.test(f) && !/\.test\.tsx?$/.test(f))

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
