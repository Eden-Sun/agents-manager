/**
 * 「Jev 建議」統一標示（issue #1912）：真的掛 `JevLabel` 進 happy-dom。
 * 寫出「Jev 建議」＋機率百分比；要求時接「只是建議，不會自動動作」；沒有機率就不寫百分比。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { jevProbText } from '../lib/jevLabel'
import { JevLabel } from './JevLabel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('寫出「Jev 建議」與機率百分比，title 說明來源與只是建議', async () => {
  const root = await mount(<JevLabel prob={0.824} />)
  assert.equal(root.querySelector('.jev-label')!.textContent, 'Jev 建議 82%')
  const title = root.querySelector<HTMLElement>('.jev-label-tag')!.title
  assert.match(title, /TypeSafe/)
  assert.match(title, /機率 82%/)
  assert.match(title, /不會自動動作/)
})

test('note：接上「只是建議，不會自動動作」', async () => {
  const root = await mount(<JevLabel prob={0.5} note />)
  assert.match(root.querySelector('.jev-label')!.textContent ?? '', /只是建議，不會自動動作/)
})

test('沒有機率或不在 0–1：不寫百分比', async () => {
  assert.equal(jevProbText(null), null)
  assert.equal(jevProbText(1.2), null)
  assert.equal(jevProbText(Number.NaN), null)
  assert.equal(jevProbText(0), '0%')
  const root = await mount(<JevLabel />)
  assert.equal(root.querySelector('.jev-label')!.textContent, 'Jev 建議')
})
