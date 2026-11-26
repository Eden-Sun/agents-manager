import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ViewableImage } from './MarkdownImage'
before(setupDom)
afterEach(unmountAll)
after(teardownDom)
async function open() {
  await mount(<div data-image-gallery><ViewableImage src="data:image/png;base64,AA==" alt="one.png" /><ViewableImage src="data:image/png;base64,AQ==" alt="two.png" /></div>)
  const opener = document.querySelector<HTMLAnchorElement>('.md-image-link')!
  await act(() => opener.click())
  return opener
}
const dialog = () => document.querySelector<HTMLElement>('.image-viewer')
const stage = () => document.querySelector<HTMLElement>('.image-viewer-stage')!
test('background closes and restores focus and body/html styles', async () => {
  document.body.style.overflow = 'auto'
  document.documentElement.style.overflow = ''
  const opener = await open()
  assert.equal(document.body.style.overflow, 'hidden')
  assert.equal(document.documentElement.style.overflow, 'hidden')
  await act(() => dialog()!.click())
  assert.equal(dialog(), null)
  assert.equal(document.activeElement, opener)
  assert.equal(document.body.style.overflow, 'auto')
  assert.equal(document.documentElement.style.overflow, '')
})
test('Escape closes the dialog', async () => {
  await open()
  await act(() => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' })))
  assert.equal(dialog(), null)
})
test('image toggles original size; arrows and buttons switch images', async () => {
  await open()
  const picture = () => document.querySelector<HTMLButtonElement>('.image-viewer-picture')!
  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'true')
  assert.ok(stage().classList.contains('zoomed'))
  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'false')
  assert.ok(!stage().classList.contains('zoomed'))
  await act(() => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight' })))
  assert.equal(picture().querySelector('img')!.alt, 'two.png')
  await act(() => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft' })))
  assert.equal(picture().querySelector('img')!.alt, 'one.png')
  await act(() => document.querySelector<HTMLButtonElement>('.image-viewer-nav button:last-child')!.click())
  assert.equal(picture().querySelector('img')!.alt, 'two.png')
})
test('zoomed stage blocks page pinch-zoom in CSS and stays zoomed after touch drag', async () => {
  const css = readFileSync(new URL('./imageViewer.css', import.meta.url), 'utf8')
  assert.doesNotMatch(css, /pinch-zoom/, '不允許瀏覽器對整頁做雙指縮放')
  assert.match(css, /\.image-viewer\s*\{[^}]*touch-action:\s*none/)
  assert.match(css, /\.image-viewer-stage\.zoomed\s*\{[^}]*overflow:\s*auto[^}]*touch-action:\s*pan-x pan-y/)

  await open()
  const picture = () => document.querySelector<HTMLButtonElement>('.image-viewer-picture')!
  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'true')

  const touchEv = (type: string, touches: Array<{ clientX: number; clientY: number }>, changed = touches) => {
    const ev = new Event(type, { bubbles: true, cancelable: true }) as Event & { touches: unknown; changedTouches: unknown }
    ev.touches = touches
    ev.changedTouches = changed
    return ev
  }
  await act(() => stage().dispatchEvent(touchEv('touchstart', [{ clientX: 200, clientY: 300 }])))
  await act(() => stage().dispatchEvent(touchEv('touchmove', [{ clientX: 120, clientY: 260 }])))
  await act(() => stage().dispatchEvent(touchEv('touchend', [], [{ clientX: 120, clientY: 260 }])))
  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'true', '放大後拖曳放開不誤觸縮回')

  const pinch = touchEv('touchmove', [{ clientX: 100, clientY: 200 }, { clientX: 220, clientY: 240 }])
  await act(() => dialog()!.dispatchEvent(pinch))
  assert.equal(pinch.defaultPrevented, true, '雙指縮放手勢被攔截，不外溢到整頁')

  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'false', '再次單純點一下縮回整張')
})
