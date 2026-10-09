import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

/**
 * issue #548：寫 `--vvh` 的那一側（`useViewportPin`）與吃它的那一側（`styles.css`）必須是同一個門檻。
 * 只放寬一邊的話另一邊會靜靜失效——原本就是 hook 走 ≤640、CSS 也走 ≤640，iPad 直式（768／820）兩邊都沒有，
 * 鍵盤一彈出來就把底部輸入列蓋住。這一條只認得出「兩邊不一致」，實機行為沒辦法在這裡驗。
 */
test('--vvh 的寫入端與使用端都掛在抽屜版面（≤1024px）', () => {
  const hook = readFileSync(new URL('./useViewportPin.ts', import.meta.url), 'utf8')
  assert.match(hook, /useMediaQuery\(DRAWER_QUERY\)/)
  assert.doesNotMatch(hook, /useMediaQuery\(PHONE_QUERY\)/, '註解可以提到它，但不能再用它當門檻')

  const css = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')
  // html/body/#root 那條（#548 的註解之後）；#933 之後手機 sheet 也有 `height: var(--vvh, 100dvh)`，不能只找第一個。
  const at = css.indexOf('height: var(--vvh, 100dvh);', css.indexOf('issue #548'))
  assert.ok(at > 0, 'styles.css 要有 html/body/#root 的 height: var(--vvh, 100dvh)')
  // 包住那一行的是哪一個 @media：往前找最近的那個門檻，它要涵蓋抽屜版面。
  const before = css.slice(0, at).match(/@media \(width <= (\d+)px\)(?![\s\S]*@media \(width <= \d+px\))/)
  assert.ok(before, '那一行要在某個 max-width 的 @media 裡')
  assert.equal(Number(before![1]), 1024, `--vvh 的高度規則掛在 ≤${before![1]}px，iPad 直式（768／820）吃不到`)
})

/** issue #933：sheet 的高度要跟 `--vvh`（鍵盤彈出時的可視高度）走，不能只用 `100dvh`（iOS 鍵盤不縮它）。 */
test('手機全螢幕 sheet 的高度與遮罩都吃 --vvh', () => {
  const css = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')
  const bs = readFileSync(new URL('../components/botSettings.css', import.meta.url), 'utf8')
  const confirm = readFileSync(new URL('../components/confirmDialog.css', import.meta.url), 'utf8')
  const rule = (src: string, selector: string) => {
    const i = src.indexOf(`${selector} {`)
    assert.ok(i >= 0, `找不到 ${selector}`)
    return src.slice(i, src.indexOf('}', i))
  }
  const mobile = css.slice(css.indexOf('對話框在 ≤640 一律全螢幕 sheet'))
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

/** issue #933：聚焦後 iOS 才動 viewport，補捲只對 sheet 內的欄位做，不去動頁面其他地方的焦點。 */
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
