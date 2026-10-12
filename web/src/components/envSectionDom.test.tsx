/**
 * 環境設定的區塊（`EnvSection`，issue #1466）：手機（≤640px）每區是一條可點的標題列、預設收著，
 * 長頁面才能快速跳到某一區；桌機維持原樣（標題 + 內容一直展開，沒有按鈕）。
 * 內容收起時用 `hidden` 而不是卸載：打了一半的表單與讀回來的狀態都留著。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { EnvSection } from './EnvSection'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

/** `matchMedia` 對 640px 斷點回 `phone`。回傳還原函式。 */
function viewport(phone: boolean): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({
    matches: phone && q.includes('max-width: 640px'),
    media: q,
    addEventListener() {},
    removeEventListener() {},
  })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

const page = (
  <div className="env-panel">
    <EnvSection title="主機">
      <input id="host-input" />
    </EnvSection>
    <EnvSection title="身分">
      <p>身分內容</p>
    </EnvSection>
  </div>
)

test('手機：每區一顆標題鍵，預設收著，點開只開那一區', async () => {
  const restore = viewport(true)
  try {
    const root = await mount(page)
    const toggles = [...root.querySelectorAll<HTMLButtonElement>('h3 > button.env-sec-toggle')]
    assert.equal(toggles.length, 2)
    assert.deepEqual(toggles.map((b) => b.getAttribute('aria-expanded')), ['false', 'false'])
    const bodies = [...root.querySelectorAll<HTMLElement>('.env-sec-body')]
    assert.ok(bodies.every((b) => b.hidden), '預設全收')
    // aria-controls 指到內容本身。
    assert.equal(toggles[0]!.getAttribute('aria-controls'), bodies[0]!.id)
    await click(toggles[0]!)
    assert.equal(toggles[0]!.getAttribute('aria-expanded'), 'true')
    assert.equal(bodies[0]!.hidden, false)
    assert.equal(toggles[1]!.getAttribute('aria-expanded'), 'false')
    assert.equal(bodies[1]!.hidden, true)
    await click(toggles[0]!)
    assert.equal(bodies[0]!.hidden, true, '再點一次收回去')
  } finally {
    restore()
  }
})

test('手機：收起的內容還在 DOM 裡（打了一半的表單不能因為收起而丟掉）', async () => {
  const restore = viewport(true)
  try {
    const root = await mount(page)
    const input = root.querySelector<HTMLInputElement>('#host-input')
    assert.ok(input, '收起時內容仍掛著')
    assert.ok(input!.closest('.env-sec-body')!.hasAttribute('hidden'))
  } finally {
    restore()
  }
})

test('桌機：沒有標題鍵，內容一直展開', async () => {
  const restore = viewport(false)
  try {
    const root = await mount(page)
    assert.equal(root.querySelectorAll('button.env-sec-toggle').length, 0)
    assert.deepEqual([...root.querySelectorAll('h3')].map((h) => h.textContent), ['主機', '身分'])
    assert.ok([...root.querySelectorAll<HTMLElement>('.env-sec-body')].every((b) => !b.hidden))
  } finally {
    restore()
  }
})
