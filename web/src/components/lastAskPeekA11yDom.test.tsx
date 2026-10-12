/**
 * 「這回合的提問」浮窗的展開鈕：只有真的被夾掉、會展開全文時才宣告 aria-expanded；
 * 沒夾掉的（點了是捲到那則）不能宣告成「收合中的折疊鈕」。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { LastAskPeek } from './ChatPanel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const ask = (content: string) => ({
  id: 'u1', conversation_id: 'c1', turn_id: 't1', bot_id: 'b1', role: 'user', content, source: 'web',
  incomplete: false, group_id: null, attachments: [], relay_from: null,
  terminal_snapshot: null, created_at: '2026-10-10T00:00:00.000Z',
})

async function renderAsk(content: string) {
  await mount(<LastAskPeek msg={ask(content) as never} turnId="t1" onJump={() => {}} />)
  return document.querySelector<HTMLButtonElement>('.lastq-body')!
}

test('沒被夾掉的提問：展開鈕不宣告 aria-expanded（點了是捲到那則，不是展開）', async () => {
  const button = await renderAsk('短問題')
  assert.ok(button, '要畫出提問浮窗')
  assert.equal(button.hasAttribute('aria-expanded'), false, '不能宣告成收合中的折疊鈕')
})

test('被夾掉的提問：收合時 aria-expanded=false，點開後變 true', async () => {
  // happy-dom 沒有版面：模擬內容高度超過可見高度，讓浮窗判定為「被夾掉」。
  const proto = HTMLElement.prototype
  Object.defineProperty(proto, 'scrollHeight', { configurable: true, get: () => 200 })
  Object.defineProperty(proto, 'clientHeight', { configurable: true, get: () => 40 })
  try {
    const button = await renderAsk('很長很長的提問'.repeat(40))
    await settle(20)
    assert.equal(button.getAttribute('aria-expanded'), 'false', '收合時宣告 false')
    await click(document.querySelector('.lastq-body')!)
    await settle(20)
    assert.equal(document.querySelector('.lastq-body')!.getAttribute('aria-expanded'), 'true', '展開後宣告 true')
  } finally {
    delete (proto as { scrollHeight?: number }).scrollHeight
    delete (proto as { clientHeight?: number }).clientHeight
  }
})
