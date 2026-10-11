/** #1246：側欄下緣的動作選單在可行時向上展開。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { HeadMoreMenu } from './HeadMoreMenu'

before(setupDom)
afterEach(unmountAll)

type Rect = { top: number; bottom: number; height: number; left: number; right: number }
let rects: { clip: Rect; button: Rect; pop: Rect }

function domRect(rect: Rect): DOMRect {
  const width = rect.right - rect.left
  return {
    ...rect,
    x: rect.left,
    y: rect.top,
    width,
    toJSON: () => rect,
  } as DOMRect
}

let nativeRect: typeof HTMLElement.prototype.getBoundingClientRect
before(() => {
  nativeRect = HTMLElement.prototype.getBoundingClientRect
  HTMLElement.prototype.getBoundingClientRect = function () {
    if (this.classList.contains('sidebar-scroll')) return domRect(rects.clip)
    if (this.classList.contains('head-menu')) return domRect(rects.button)
    if (this.classList.contains('head-menu-pop')) return domRect(rects.pop)
    return nativeRect.call(this)
  }
})
after(async () => {
  HTMLElement.prototype.getBoundingClientRect = nativeRect
  await teardownDom()
})

async function openMenu() {
  await mount(
    <div className="sidebar-scroll">
      <HeadMoreMenu label="動作">
        <button role="menuitem">刪除…</button>
      </HeadMoreMenu>
    </div>,
  )
  await act(() => document.querySelector<HTMLButtonElement>('.head-menu-btn')!.click())
  return document.querySelector<HTMLElement>('.head-menu-pop')!
}

test('貼著下緣時往上開', async () => {
  rects = {
    clip: { top: 0, bottom: 600, height: 600, left: 0, right: 320 },
    button: { top: 570, bottom: 596, height: 26, left: 100, right: 126 },
    pop: { top: 602, bottom: 760, height: 158, left: 100, right: 268 },
  }
  Object.defineProperty(window, 'innerHeight', { configurable: true, value: 800 })
  const pop = await openMenu()
  assert.ok(pop.hasAttribute('data-up'), '選單應向上展開以避開側欄下緣')
})

test('上方空間夠時照舊往下', async () => {
  rects = {
    clip: { top: 0, bottom: 600, height: 600, left: 0, right: 320 },
    button: { top: 100, bottom: 126, height: 26, left: 100, right: 126 },
    pop: { top: 132, bottom: 290, height: 158, left: 100, right: 268 },
  }
  const pop = await openMenu()
  assert.equal(pop.hasAttribute('data-up'), false)
})

test('上下都放不下時不翻', async () => {
  rects = {
    clip: { top: 0, bottom: 150, height: 150, left: 0, right: 320 },
    button: { top: 60, bottom: 86, height: 26, left: 100, right: 126 },
    pop: { top: 92, bottom: 250, height: 158, left: 100, right: 268 },
  }
  const pop = await openMenu()
  assert.equal(pop.hasAttribute('data-up'), false)
})
