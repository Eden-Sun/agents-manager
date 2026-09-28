import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

const dir = new URL('.', import.meta.url).pathname

for (const [file, role] of [
  ['ConfirmDialog.tsx', 'alertdialog'],
  ['Modal.tsx', 'dialog'],
] as const) {
  test(`${file} 根節點可接住框內文字點擊的焦點，Esc 仍能辨認對話框歸屬`, () => {
    const source = readFileSync(join(dir, file), 'utf8')
    const root = source.match(new RegExp(`<div\\b(?=[^>]*role="${role}")[^>]*>`))

    assert.ok(root, `找不到 role="${role}" 的根節點`)
    assert.match(root[0], /\btabIndex=\{-1\}/)
  })
}
