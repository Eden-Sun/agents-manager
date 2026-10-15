import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const css = await readFile(new URL('./codexResetHint.css', import.meta.url), 'utf8')

/**
 * #1776：桌機額度收合時，codex 的 head 不能是 size container。
 * `.quota-hp.codex > .quota-head` 寫了 `container-type: inline-size`，內在寬度會算成 0；
 * 收合規則把 `.quota-head` 改成 `width: auto` 後，codex 名字就溢出疊到右邊的量表上。
 * 收合時重置徽章的文字標籤本來就不顯示（只看 ≥110px 的 container），所以收合那段改回 normal。
 */
test('桌機收合時 codex 的 head 取消 container-type（寬度 auto 才量得到名字，#1776）', () => {
  const m = css.match(
    /@media \(width > 640px\) \{\s*\n\s*\.main-head \.quota-open\.collapsed \.quota-hp\.codex > \.quota-head \{([^}]*)\}/,
  )
  assert.ok(m, 'codexResetHint.css 要有 @media (width > 640px) 內的收合 codex head 規則')
  assert.match(m[1], /container-type:\s*normal;/)
})
