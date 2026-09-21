import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { test } from 'node:test'

const VALUE_EXPORT = /^\s*export\s+(?:(?:default|async)\s+)*(?:function|class|const|let|var)\s+([A-Za-z_$][\w$]*)/gm

const isComponentName = (name: string) => /^[A-Z][A-Za-z0-9]*$/.test(name) && /[a-z]/.test(name)

test('PascalCase component files only export components or types', () => {
  const dir = new URL('.', import.meta.url).pathname
  const bad: string[] = []
  for (const file of readdirSync(dir).filter((name) => /^[A-Z].*\.tsx$/.test(name))) {
    const source = readFileSync(join(dir, file), 'utf8')
    for (const match of source.matchAll(VALUE_EXPORT)) {
      const name = match[1]
      if (!isComponentName(name)) bad.push(`${file}:${source.slice(0, match.index).split('\n').length} ${name}`)
    }
  }
  assert.deepEqual(bad, [])
})
