import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'

const ROOT = new URL('.', import.meta.url).pathname
const BARE = /\bfetch\(\s*['"`]\/api\b/

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name)
    if (statSync(p).isDirectory()) return sources(p)
    return /\.tsx?$/.test(name) && !/\.test\.tsx?$/.test(name) ? [p] : []
  })
}

test('web/src/api 只有 transport.ts 能直接 fetch /api（其他走 transport 才會帶 UI token）', () => {
  const offenders = sources(ROOT)
    .filter((p) => relative(ROOT, p) !== 'transport.ts')
    .flatMap((p) =>
      readFileSync(p, 'utf8')
        .split('\n')
        .flatMap((line, i) => (BARE.test(line) ? [`${relative(ROOT, p)}:${i + 1}: ${line.trim()}`] : [])),
    )
  assert.deepEqual(offenders, [], `改走 rawTransport.request：\n${offenders.join('\n')}`)
})

test('守門的比對式抓得到三種引號', () => {
  for (const s of ["fetch('/api/x')", 'fetch("/api/x")', 'fetch(`/api${p}`)']) assert.ok(BARE.test(s), s)
  assert.ok(!BARE.test("rawTransport.request('GET', '/x')"))
})
