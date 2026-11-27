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
const touchEv = (type: string, touches: Array<{ clientX: number; clientY: number }>, changed = touches) => {
  const ev = new Event(type, { bubbles: true, cancelable: true }) as Event & { touches: unknown; changedTouches: unknown }
  ev.touches = touches
  ev.changedTouches = changed
  return ev
}
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
test('image toggles zoom; arrows and buttons switch images', async () => {
  await open()
  const picture = () => document.querySelector<HTMLButtonElement>('.image-viewer-picture')!
  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'true')
  assert.ok(stage().classList.contains('zoomed'))
  assert.match(picture().style.transform, /scale\(2\)/)
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
test('pinch-to-zoom scales image up to 5x while blocking page pinch-zoom, and stays zoomed after drag', async () => {
  const css = readFileSync(new URL('./imageViewer.css', import.meta.url), 'utf8')
  assert.doesNotMatch(css, /pinch-zoom/, '不允許瀏覽器對整頁做雙指縮放')
  assert.match(css, /\.image-viewer\s*\{[^}]*touch-action:\s*none/)
  assert.match(css, /\.image-viewer-stage\s*\{[^}]*touch-action:\s*none/)

  await open()
  const picture = () => document.querySelector<HTMLButtonElement>('.image-viewer-picture')!

  await act(() => stage().dispatchEvent(touchEv('touchstart', [{ clientX: 150, clientY: 200 }, { clientX: 250, clientY: 200 }])))
  const pinchMove = touchEv('touchmove', [{ clientX: 50, clientY: 200 }, { clientX: 350, clientY: 200 }])
  await act(() => stage().dispatchEvent(pinchMove))
  assert.equal(pinchMove.defaultPrevented, true, '雙指縮放手勢被攔截，不外溢到整頁')
  assert.equal(picture().getAttribute('aria-pressed'), 'true')
  assert.match(picture().style.transform, /scale\(3\)/, '兩指拉大 3 倍即放大至 3×')
  assert.equal(document.querySelector('.image-viewer-scale')?.textContent, '3.0×')
  await act(() => stage().dispatchEvent(touchEv('touchend', [], [{ clientX: 50, clientY: 200 }, { clientX: 350, clientY: 200 }])))

  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'true', '雙指放開後的殘留 click 不誤觸縮回')

  await act(() => stage().dispatchEvent(touchEv('touchstart', [{ clientX: 200, clientY: 300 }])))
  await act(() => stage().dispatchEvent(touchEv('touchmove', [{ clientX: 120, clientY: 260 }])))
  await act(() => stage().dispatchEvent(touchEv('touchend', [], [{ clientX: 120, clientY: 260 }])))
  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'true', '放大後拖曳放開不誤觸縮回')

  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'false', '再次單純點一下縮回整張')
  assert.equal(document.querySelector('.image-viewer-scale'), null)
})
