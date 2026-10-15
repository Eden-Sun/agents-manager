import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { message, notice } from '../testing/childNoticeFixtures'
import { ChildNoticeMessages } from './ChildNoticeMessages'
import { Bubble } from './ChatPanel'
import { useStore } from '../store/store'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

test('收合只一行；點一次同時展開通知全文、parent 回覆，再點收起', async () => {
  useStore.setState({ bots: [], turns: {} })
  const msg = notice('t')
  msg.content += '\n' + '原文內容'.repeat(100)
  await mount(<ChildNoticeMessages messages={[msg, message('a', 't', 'assistant', 'parent 回覆全文')]} renderMessage={(m, expanded) => <Bubble msg={m} expandRelay={expanded} />} />)
  const button = document.querySelector<HTMLButtonElement>('.child-notice-toggle')!
  assert.equal(button.getAttribute('aria-expanded'), 'false')
  assert.match(button.textContent!, /↳ child sc1779 回報完成/)
  assert.equal(document.querySelector('.msg'), null)
  await click(button)
  assert.equal(button.getAttribute('aria-expanded'), 'true')
  assert.equal(document.querySelectorAll('.msg').length, 2)
  assert.equal(document.querySelector('.msg.user .bubble')!.textContent, msg.content)
  assert.match(document.querySelector('.msg.assistant')!.textContent!, /parent 回覆全文/)
  assert.equal(document.querySelector('.relay-fold'), null)
  await click(button)
  assert.equal(document.querySelector('.msg'), null)
})

test('連續通知共一列，使用者自己的訊息與普通回覆始終可見', async () => {
  const msgs = [notice('t'), message('a', 't', 'assistant'), notice('t2', 'qs1780'), message('a2', 't2', 'assistant'),
    message('own', 'u', 'user', '我自己的提問'), message('answer', 'u', 'assistant', '普通回覆')]
  await mount(<ChildNoticeMessages messages={msgs} renderMessage={(m) => <p data-role={m.role}>{m.content}</p>} />)
  assert.equal(document.querySelectorAll('.child-notice-toggle').length, 1)
  assert.match(document.querySelector('.child-notice-toggle')!.textContent!, /sc1779、qs1780 回報完成（2）/)
  assert.equal(document.querySelector('[data-role="user"]')!.textContent, '我自己的提問')
  assert.equal(document.querySelector('[data-role="assistant"]')!.textContent, '普通回覆')
})

test('通知觸發的串流回覆收在同一列；一般串流照舊可見', async () => {
  await mount(<ChildNoticeMessages messages={[notice('t', 'x', true)]} renderMessage={(m) => <p>{m.content}</p>} liveTurnId="t" liveReply={<p data-live>串流回覆</p>} />)
  assert.equal(document.querySelector('[data-live]'), null)
  await click(document.querySelector('.child-notice-toggle')!)
  assert.ok(document.querySelector('[data-live]'))
  assert.match(document.querySelector('.child-notice-toggle')!.textContent!, /在等人回答/)
  await unmountAll()
  await mount(<ChildNoticeMessages messages={[message('u', 'normal', 'user')]} renderMessage={(m) => <p>{m.content}</p>} liveTurnId="normal" liveReply={<p data-live>普通串流</p>} />)
  assert.ok(document.querySelector('[data-live]'))
})
