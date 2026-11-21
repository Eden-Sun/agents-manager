import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./codexResetHint.css', import.meta.url), 'utf8')

test('桌機收合時 codex 的 head 取消 container-type（寬度 auto 才量得到名字，#1776）', () => {
  const m = css.match(
    /@media \(width > 640px\) \{\s*\n\s*\.main-head \.quota-open\.collapsed \.quota-hp\.codex > \.quota-head \{([^}]*)\}/,
  )
  assert.ok(m, 'codexResetHint.css 要有 @media (width > 640px) 內的收合 codex head 規則')
  assert.match(m[1], /container-type:\s*normal;/)
})
