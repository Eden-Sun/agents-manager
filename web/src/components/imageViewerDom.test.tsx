import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
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
test('background closes and restores focus and body styles', async () => {
  document.body.style.overflow = 'auto'
  const opener = await open()
  assert.equal(document.body.style.overflow, 'hidden')
  await act(() => dialog()!.click())
  assert.equal(dialog(), null)
  assert.equal(document.activeElement, opener)
  assert.equal(document.body.style.overflow, 'auto')
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
  await act(() => picture().click())
  assert.equal(picture().getAttribute('aria-pressed'), 'false')
  await act(() => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight' })))
  assert.equal(picture().querySelector('img')!.alt, 'two.png')
  await act(() => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft' })))
  assert.equal(picture().querySelector('img')!.alt, 'one.png')
  await act(() => document.querySelector<HTMLButtonElement>('.image-viewer-nav button:last-child')!.click())
  assert.equal(picture().querySelector('img')!.alt, 'two.png')
})
