import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ImageShelf } from './ImageShelf'
import { MobilePreview } from './MobilePreview'
import {
  previewScaleFor,
  resetMobilePreviewForTest,
  setMobilePreviewDevice,
  setMobilePreviewOpen,
  useMobilePreviewDevice,
} from '../store/mobilePreview'
import { TABLET_QUERY } from '../hooks/useMediaQuery'

let realMatchMedia: typeof window.matchMedia
let mockWidth = 1600

function installMatchMedia(width: number) {
  mockWidth = width
  window.matchMedia = ((query: string) => {
    const matches = (() => {
      if (query === TABLET_QUERY) return mockWidth >= 641 && mockWidth <= 1366
      const maxM = query.match(/max-width:\s*(\d+)px/)
      const minM = query.match(/min-width:\s*(\d+)px/)
      const okMax = maxM ? mockWidth <= Number(maxM[1]) : true
      const okMin = minM ? mockWidth >= Number(minM[1]) : true
      return okMax && okMin
    })()
    return {
      matches,
      media: query,
      onchange: null,
      addEventListener() {},
      removeEventListener() {},
      addListener() {},
      removeListener() {},
      dispatchEvent() { return false },
    } as MediaQueryList
  }) as typeof window.matchMedia
}

before(async () => {
  await setupDom()
  realMatchMedia = window.matchMedia
})

afterEach(async () => {
  await unmountAll()
  localStorage.clear()
  resetMobilePreviewForTest()
  installMatchMedia(1600)
})

after(async () => {
  window.matchMedia = realMatchMedia
  await teardownDom()
})

test('平板寬度（834px／1194px）下 File shelf 預設收合；桌面（1600px）預設展開', async () => {
  installMatchMedia(834)
  await mount(<ImageShelf />)
  assert.equal(document.querySelector('.shelf.open'), null, 'iPad 直向預設收合')
  assert.ok(document.querySelector('.shelf-handle'))

  await unmountAll()
  installMatchMedia(1194)
  await mount(<ImageShelf />)
  assert.equal(document.querySelector('.shelf.open'), null, 'iPad 橫向預設收合')

  await unmountAll()
  installMatchMedia(1600)
  await mount(<ImageShelf />)
  assert.ok(document.querySelector('.shelf.open'), '桌面預設展開')
})

test('右側預覽可切換 iPad 直向／橫向並記住於 localStorage，縮放比例依寬度自動算', async () => {
  installMatchMedia(1600)
  setMobilePreviewOpen(true)
  setMobilePreviewDevice('phone')
  await mount(<MobilePreview />)

  const aside = () => document.querySelector<HTMLElement>('.mobile-preview')!
  assert.equal(aside().dataset.device, 'phone')

  const buttons = [...document.querySelectorAll<HTMLButtonElement>('.mp-dev-btn')]
  assert.equal(buttons.length, 3)

  await click(buttons[1])
  assert.equal(aside().dataset.device, 'ipad-portrait')
  assert.equal(localStorage.getItem('am.mobilePreview.device'), 'ipad-portrait')
  assert.equal(document.querySelector<HTMLIFrameElement>('.mp-frame')!.style.width, '834px')
  assert.equal(document.querySelector<HTMLIFrameElement>('.mp-frame')!.style.height, '1194px')

  await click(buttons[2])
  assert.equal(aside().dataset.device, 'ipad-landscape')
  assert.equal(localStorage.getItem('am.mobilePreview.device'), 'ipad-landscape')
  assert.equal(document.querySelector<HTMLIFrameElement>('.mp-frame')!.style.width, '1194px')
  assert.equal(document.querySelector<HTMLIFrameElement>('.mp-frame')!.style.height, '834px')

  await unmountAll()
  resetMobilePreviewForTest()
  setMobilePreviewOpen(true)
  await mount(<MobilePreview />)
  assert.equal(aside().dataset.device, 'ipad-landscape', '重新掛載後保留上次選的裝置')
  assert.ok(previewScaleFor('ipad-portrait', 380, 520) < 0.5)
  assert.ok(previewScaleFor('ipad-landscape', 380, 520) <= 380 / 1194 + 0.002)
  assert.equal(typeof useMobilePreviewDevice, 'function')
})

test('平板與手機斷點將 toast 移到上方，不遮住底部輸入框與送出鈕', () => {
  const css = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')
  assert.match(css, /@media \(width <= 1366px\)\s*\{\s*\.notices\s*\{[^}]*top:\s*calc\(env\(safe-area-inset-top\)\s*\+\s*64px\)[^}]*bottom:\s*auto/)
})
