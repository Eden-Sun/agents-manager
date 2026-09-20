import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'

// 自己刻的 aria-modal 對話框不會有 focus trap、Esc、關閉還原焦點；一律走 useDialogFocus／ConfirmDialog／Modal。
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
