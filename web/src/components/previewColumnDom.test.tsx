/**
 * 桌機預覽欄的寬度把手（issue #1137）：可聚焦的 separator 要有完整的 value 範圍，Home／End 直接到最窄／最寬。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, setupDom, teardownDom, unmountAll, settle } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { setPreviewColOpen } from '../store/previewLayout'
import { PreviewColumn } from './PreviewColumn'

const VIEWPORT = 1600
const originalMatchMedia = window.matchMedia
const originalInnerWidth = window.innerWidth

before(setupDom)
afterEach(async () => {
  await unmountAll()
  window.matchMedia = originalMatchMedia
  Object.defineProperty(window, 'innerWidth', { configurable: true, value: originalInnerWidth })
  localStorage.removeItem('am.previewCol.openBots')
  localStorage.removeItem('am.previewCol.width')
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

/** 桌機寬度：比 1024px 斷點寬，抽屜不啟用。 */
function desktop() {
  Object.defineProperty(window, 'innerWidth', { configurable: true, value: VIEWPORT })
  window.matchMedia = ((q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
}

const grip = () => document.querySelector<HTMLElement>('[role="separator"]')!

test('寬度把手有完整的 value 範圍', async () => {
  desktop()
  fakeApi(() => ({}))
  useStore.setState({
    selectedProjectId: null,
    selectedBotId: 'b1',
    bots: [{ id: 'b1', name: 'top', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null } as never],
    previews: {},
  } as never)
  setPreviewColOpen('b1', true)
  await mount(<PreviewColumn />)
  await settle(50)
  assert.equal(grip().getAttribute('aria-valuemin'), '320')
  assert.equal(grip().getAttribute('aria-valuemax'), '800', '視窗 1600 的一半')
  const now = Number(grip().getAttribute('aria-valuenow'))
  assert.ok(now >= 320 && now <= 800, `valuenow ${now} 在範圍內`)
})

test('Home／End 直接到最窄／最寬', async () => {
  desktop()
  fakeApi(() => ({}))
  useStore.setState({
    selectedProjectId: null,
    selectedBotId: 'b1',
    bots: [{ id: 'b1', name: 'top', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null } as never],
    previews: {},
  } as never)
  setPreviewColOpen('b1', true)
  await mount(<PreviewColumn />)
  await settle(50)
  grip().dispatchEvent(new KeyboardEvent('keydown', { key: 'End', bubbles: true, cancelable: true }))
  await settle(50)
  assert.equal(grip().getAttribute('aria-valuenow'), '800')
  grip().dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }))
  await settle(50)
  assert.equal(grip().getAttribute('aria-valuenow'), '320')
})
