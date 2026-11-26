import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const css = readFileSync(new URL('./projectSettingsPanel.css', import.meta.url), 'utf8')

const rule = (selector: string) => {
  const m = new RegExp(`(^|\\n)${selector.replace(/[.]/g, '\\.')}\\s*\\{([^}]*)\\}`).exec(css)
  assert.ok(m, `缺少 ${selector} 規則`)
  return m[2]
}

test('Save 條黏在捲動區真正的底緣，不在 padding 上方留縫讓卡片透出', () => {
  const panel = rule('.project-settings')
  const pad = /--project-settings-pad:\s*([^;]+);/.exec(panel)?.[1]
  assert.equal(pad, '24px')
  assert.match(panel, /padding:\s*var\(--project-settings-pad\)/)
  const bar = rule('.project-savebar')
  assert.match(bar, /position:\s*sticky/)
  assert.match(bar, /bottom:\s*calc\(-1 \* var\(--project-settings-pad/)
})

test('Save 條底部含 safe-area，捲動區保留 scroll-padding 讓聚焦的欄位不被蓋住', () => {
  assert.match(rule('.project-savebar'), /padding:[^;]*env\(safe-area-inset-bottom/)
  assert.match(rule('.project-settings'), /scroll-padding-bottom:[^;]*env\(safe-area-inset-bottom/)
})
