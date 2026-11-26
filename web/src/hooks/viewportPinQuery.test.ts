import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

test('--vvh 與整頁鎖捲的寫入端與使用端都涵蓋手機與 iPad 直／橫向（≤1366px）', () => {
  const hook = readFileSync(new URL('./useViewportPin.ts', import.meta.url), 'utf8')
  assert.match(hook, /useMediaQuery\(VIEWPORT_PIN_QUERY\)/)
  assert.doesNotMatch(hook, /useMediaQuery\(PHONE_QUERY\)/, '註解可以提到它，但不能再用它當門檻')

  const css = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')
  const start = css.indexOf('@media (width <= 1366px)')
  assert.ok(start >= 0, 'styles.css 要有 @media (width <= 1366px)')
  const at = css.indexOf('height: var(--vvh, 100dvh);', start)
  assert.ok(at > 0, 'styles.css 要有 html/body/#root 的 height: var(--vvh, 100dvh)')
  const before = css.slice(0, at).match(/@media \(width <= (\d+)px\)(?![\s\S]*@media \(width <= \d+px\))/)
  assert.ok(before, '那一行要在某個 max-width 的 @media 裡')
  assert.equal(Number(before![1]), 1366, `--vvh 的高度規則掛在 ≤${before![1]}px，iPad 橫式（1194／1366）吃不到`)
  const block = css.slice(at, css.indexOf('@media (width <= 640px)', at))
  assert.match(block, /html,\s*body\s*\{[^}]*position:\s*fixed;[^}]*overflow:\s*hidden;[^}]*overscroll-behavior:\s*none;/, 'iPad 直／橫向也要鎖定 html/body 防整頁捲動')
})

test('手機全螢幕 sheet 的高度與遮罩都吃 --vvh', () => {
  const css = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')
  const bs = readFileSync(new URL('../components/botSettings.css', import.meta.url), 'utf8')
  const confirm = readFileSync(new URL('../components/confirmDialog.css', import.meta.url), 'utf8')
  const rule = (src: string, selector: string) => {
    const i = src.indexOf(`${selector} {`)
    assert.ok(i >= 0, `找不到 ${selector}`)
    return src.slice(i, src.indexOf('}', i))
  }
  const mobile = css.slice(css.indexOf('@media (width <= 640px)'))
  assert.match(mobile, /\n  \.modal-backdrop \{\s*bottom: auto;\s*height: var\(--vvh, 100dvh\)/)
  assert.match(rule(mobile, '.modal,\n  .modal:has(.dirpicker)'), /height: var\(--vvh, 100dvh\)/)
  const blocked = css.slice(css.lastIndexOf('.modal.blocked-modal {'))
  assert.match(blocked.slice(0, blocked.indexOf('}')), /height: var\(--vvh, 100dvh\)/)
  const bsMobile = bs.slice(bs.indexOf('@media (width <= 640px)'))
  assert.match(rule(bsMobile, '.bs-scrim'), /bottom: auto;\s*height: var\(--vvh, 100dvh\)/)
  assert.match(rule(bsMobile, '.bot-settings,\n  .bot-settings.anchored'), /inset: 0 0 auto 0;\s*height: var\(--vvh, 100dvh\)/)
  const confirmMobile = confirm.slice(confirm.indexOf('@media (width <= 640px)'))
  assert.match(confirmMobile, /max-height: calc\(100vh - 24px\);\s*max-height: calc\(100dvh - 24px\);\s*max-height: calc\(var\(--vvh, 100dvh\) - 24px\)/, '舊瀏覽器 fallback 兩行保留')
})

test('鍵盤彈出後的補捲只對 sheet 內的焦點欄位呼叫 scrollIntoView', async () => {
  const { revealInSheet, SHEET_SELECTOR } = await import('./useViewportPin.ts')
  const mk = (insideSheet: boolean) => {
    const calls: unknown[] = []
    const el = {
      closest: (sel: string) => (insideSheet && sel === SHEET_SELECTOR ? {} : null),
      scrollIntoView: (opts: unknown) => calls.push(opts),
    } as unknown as Element
    return { el, calls }
  }
  const inside = mk(true)
  assert.equal(revealInSheet(inside.el), true)
  assert.deepEqual(inside.calls, [{ block: 'nearest' }])
  const outside = mk(false)
  assert.equal(revealInSheet(outside.el), false)
  assert.deepEqual(outside.calls, [], '頁面其他地方的焦點不動')
  assert.equal(revealInSheet(null), false)
  assert.match(SHEET_SELECTOR, /\.modal/)
  assert.match(SHEET_SELECTOR, /\.bot-settings/)
  assert.match(SHEET_SELECTOR, /\.confirm-dialog/)

  const hook = readFileSync(new URL('./useViewportPin.ts', import.meta.url), 'utf8')
  assert.match(hook, /setTimeout\(\(\) => \{\s*apply\(\)\s*revealInSheet\(document\.activeElement\)\s*\}, 600\)/, '最後一次 apply（600ms）之後才補捲')
})
