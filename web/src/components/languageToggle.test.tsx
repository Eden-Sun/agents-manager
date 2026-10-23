/** 介面語言切換（issue #1932）：環境設定的三選一、切換後已掛載的元件即時重繪、html lang 與儲存跟著換。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, keydown, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { getLang, getLangPref, resetLangForTest, setLangPref, useT } from '../lib/i18n'
import { KeepAwakeToggle } from './KeepAwakeToggle'
import { LanguageToggle } from './LanguageToggle'
import { ThemeToggle } from './ThemeToggle'

before(setupDom)
after(teardownDom)
afterEach(async () => {
  await unmountAll()
  localStorage.clear()
  resetLangForTest()
})

const radio = (root: ParentNode, pref: string) => root.querySelector<HTMLElement>(`[data-pref="${pref}"]`)!

test('語言切換：三選一、預設跟隨瀏覽器；選項名稱用各語言自稱', async () => {
  const root = await mount(<LanguageToggle />)
  assert.deepEqual([...root.querySelectorAll('[role="radio"]')].map((b) => b.textContent), ['跟隨瀏覽器', '繁體中文', 'English'])
  assert.equal(radio(root, 'system').getAttribute('aria-checked'), 'true')
  assert.equal(radio(root, 'system').tabIndex, 0)
  assert.equal(radio(root, 'en').tabIndex, -1)
})

test('選 English：同一棵樹裡的元件立即換成英文、html lang 與儲存跟著換；再選跟隨瀏覽器就清掉', async () => {
  const root = await mount(
    <>
      <LanguageToggle />
      <KeepAwakeToggle />
      <ThemeToggle />
    </>,
  )
  assert.match(root.textContent ?? '', /螢幕保持亮著/)
  assert.equal(root.querySelector('.theme-toggle')!.getAttribute('aria-label'), '主題：跟隨系統')

  await click(radio(root, 'en'))
  assert.equal(getLang(), 'en')
  assert.equal(document.documentElement.lang, 'en')
  assert.equal(localStorage.getItem('am-lang'), 'en')
  assert.match(root.textContent ?? '', /Keep the screen on/)
  assert.equal(root.querySelector('.theme-toggle')!.getAttribute('aria-label'), 'Theme: Follow system')
  assert.deepEqual([...root.querySelectorAll('[role="radio"]')].map((b) => b.textContent), ['Follow browser', '繁體中文', 'English'])
  assert.equal(radio(root, 'en').getAttribute('aria-checked'), 'true')

  await click(radio(root, 'system'))
  assert.equal(getLang(), 'zh-TW', '測試環境的瀏覽器語言固定繁中')
  assert.equal(localStorage.getItem('am-lang'), null)
  assert.equal(document.documentElement.lang, 'zh-Hant')
  assert.match(root.textContent ?? '', /螢幕保持亮著/)
})

test('方向鍵在三個選項間循環並移動焦點', async () => {
  const root = await mount(<LanguageToggle />)
  await keydown(radio(root, 'system'), 'ArrowRight')
  assert.equal(getLangPref(), 'zh-TW')
  await keydown(radio(root, 'zh-TW'), 'ArrowRight')
  assert.equal(getLangPref(), 'en')
  await keydown(radio(root, 'en'), 'ArrowRight')
  assert.equal(getLangPref(), 'system')
  await keydown(radio(root, 'system'), 'ArrowLeft')
  assert.equal(getLangPref(), 'en')
})

test('t.x：句子裡塞元件，英文語序不同也能放對位置；切語言即時重繪', async () => {
  function Demo() {
    const t = useT()
    // 借用 ThemeToggle 已翻好的句子（字典的 key 要真的在原始碼裡）。
    return <p>{t.x('主題：{name}（點一下換成{next}）', { name: <b>A</b>, next: <i>B</i> })}</p>
  }
  const root = await mount(<Demo />)
  assert.equal(root.querySelector('p')!.innerHTML, '主題：<b>A</b>（點一下換成<i>B</i>）')
  await act(async () => setLangPref('en'))
  assert.equal(root.querySelector('p')!.innerHTML, 'Theme: <b>A</b> (click to switch to <i>B</i>)')
})

test('偏好存在 localStorage：重新載入（重設狀態）後讀回', async () => {
  setLangPref('en')
  resetLangForTest()
  assert.equal(getLang(), 'en')
  assert.equal(getLangPref(), 'en')
})
