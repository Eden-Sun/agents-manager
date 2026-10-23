/** 聊天室系統訊息：Jev 留的（開頭「Jev 建議」）換成統一標示；別的訊息原樣（issue #1912）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { JevNoticeText } from './JevNoticeText'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('Jev 的系統訊息：標「Jev 建議 82%」＋只是建議，正文跟在後面', async () => {
  const root = await mount(<JevNoticeText text="Jev 建議 82%：交辦 a1 的回報沒有驗證證據" />)
  assert.equal(root.querySelector('.jev-label-tag')?.textContent, 'Jev 建議 82%')
  assert.match(root.textContent ?? '', /只是建議，不會自動動作/)
  assert.match(root.textContent ?? '', /交辦 a1 的回報沒有驗證證據/)
})

test('別的系統訊息：原文，沒有 Jev 標示', async () => {
  const root = await mount(<JevNoticeText text="daemon 重啟完成" />)
  assert.equal(root.querySelector('.jev-label'), null)
  assert.equal(root.textContent, 'daemon 重啟完成')
})
