import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'

test('宣告 aria-modal="true" 的元件都接了 useDialogFocus', () => {
  const dir = new URL('.', import.meta.url).pathname
  const bad = readdirSync(dir)
    .filter((f) => f.endsWith('.tsx'))
    .filter((f) => {
      const s = readFileSync(join(dir, f), 'utf8')
      return /aria-modal=["{]/.test(s) && !s.includes('useDialogFocus')
    })
  assert.deepEqual(bad, [])
})
