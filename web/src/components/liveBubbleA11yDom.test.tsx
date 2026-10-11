/**
 * #1237：回覆串流中的 `LiveBubble` 不能整顆當 live region。字數計數與活動列每 250ms 變一次，
 * 讀屏會一直重唸；讀屏只該聽到「等待 → 輸出」這個階段轉換一次。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { setupDom, teardownDom, mount, unmountAll } from '../testing/domHarness'
import { LiveBubble } from './ChatPanel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('LiveBubble 本身不是 live region：整顆 article 不帶 aria-live', async () => {
  await mount(<LiveBubble text="abc" />)
  const article = document.querySelector('article.msg.live')!
  assert.ok(article, '要畫出 LiveBubble')
  assert.equal(article.hasAttribute('aria-live'), false)
})

test('讀屏只聽到階段、聽不到計數：階段字是 role=status，字數按鈕不在任何 live region 裡', async () => {
  await mount(<LiveBubble text="abc" activity="Thinking… (12s · ↑ 1.2k tokens)" />)
  const stage = document.querySelector('.sr-only[role="status"]')
  assert.ok(stage, '要有只給讀屏的階段字')
  assert.equal(stage!.textContent, '正在輸出回覆')
  assert.doesNotMatch(stage!.textContent ?? '', /\d|字/)
  const peek = document.querySelector('button.live-peek')!
  assert.ok(peek, '看目前內容的按鈕仍在')
  assert.equal(peek.closest('[aria-live], [role="status"]'), null)
})

test('等待階段不帶活動列的秒數：階段字是「等待回覆」，看得到的活動摘要照顯示', async () => {
  await mount(<LiveBubble text={null} activity="Thinking… (12s)" />)
  const stage = document.querySelector('.sr-only[role="status"]')
  assert.equal(stage!.textContent, '等待回覆')
  assert.doesNotMatch(stage!.textContent ?? '', /12s/)
  const visible = [...document.querySelectorAll('.msg-meta-left span')].find((el) => !el.classList.contains('sr-only'))
  assert.equal(visible?.textContent, 'Thinking… (12s)')
})
