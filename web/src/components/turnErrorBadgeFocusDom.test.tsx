/**
 * 回合中斷徽章的浮層（role="dialog"）：打開時焦點要進浮層，讀屏才知道多了一個對話框、按鈕才夠得到；
 * Esc 或送出後收起來，焦點回到徽章鈕，不掉到 body（同 JudgeBadge）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, focusLikeBrowserClick, keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { TurnErrorBadge } from './TurnErrorBadge'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const userMessage = {
  id: 'u1', conversation_id: 'c1', turn_id: 't1', bot_id: 'b1', role: 'user', content: '看圖', source: 'web',
  incomplete: false, group_id: null, attachments: [], relay_from: null,
  terminal_snapshot: null, created_at: '2026-10-10T00:00:00.000Z',
}

function seed() {
  fakeApi((req: FakeRequest) => {
    if (req.method === 'POST' && req.path.endsWith('/bots/b1/prompt')) {
      return { turn_id: 't2', message_id: 'u2', delivery: 'pending', send_now: null }
    }
    return undefined
  })
  useStore.setState({
    bots: [{ id: 'b1', name: 'agent', project_id: 'p1', kind: 'claude', identity: null, model: null }],
    messages: { b1: [userMessage] },
    runs: { b1: { turn_error: 'API Error: Connection error.', agent_status: 'idle' } },
  } as never)
}

async function openBadge() {
  await mount(<TurnErrorBadge botId="b1" />)
  const badge = document.querySelector<HTMLButtonElement>('.turn-error-badge')!
  focusLikeBrowserClick(badge)
  await click(badge)
  await settle(20)
  return { badge, pop: document.querySelector<HTMLElement>('.turn-error-pop') }
}

test('打開後焦點進浮層（role=dialog 要有人接住焦點）', async () => {
  seed()
  const { pop } = await openBadge()
  assert.ok(pop, '點了要開浮層')
  assert.ok(pop.contains(document.activeElement), '焦點要在浮層裡，不能還停在徽章鈕或掉到 body')
})

test('Esc 收起浮層，焦點回到徽章鈕（不掉到 body）', async () => {
  seed()
  const { badge, pop } = await openBadge()
  // 前提：焦點已在浮層裡（使用者 Tab 進來或焦點進浮層）；這裡只測收起時的還焦點。
  pop!.querySelector<HTMLElement>('button')!.focus()
  await keydown(document.activeElement!, 'Escape')
  await settle(20)
  assert.equal(document.querySelector('.turn-error-pop'), null, 'Esc 要收起浮層')
  assert.equal(document.activeElement, badge, '焦點回到徽章鈕')
})

test('重送上一則成功後收起，焦點回到徽章鈕', async () => {
  seed()
  const { badge, pop } = await openBadge()
  pop!.querySelector<HTMLElement>('button')!.focus()
  const resend = [...document.querySelectorAll<HTMLButtonElement>('.turn-error-pop button')]
    .find((b) => b.textContent?.trim() === '重送上一則')!
  await click(resend)
  await settle(50)
  assert.equal(document.querySelector('.turn-error-pop'), null, '送出成功要收起浮層')
  assert.equal(document.activeElement, badge, '焦點回到徽章鈕，不掉到 body')
})
