/**
 * 對話裡的「翻譯」鈕：英文 assistant 訊息有、中文沒有；點了打 daemon，譯文顯示在原文下方，可切換顯示。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Message } from '../api/types'
import { useMessageTranslation } from './useMessageTranslation'
import { resetTranslateReady } from './useTranslateReady'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetTranslateReady()
})
after(teardownDom)

const msg = (content: string, over: Partial<Message> = {}): Message => ({
  id: 'm1', conversation_id: 'c', turn_id: null, bot_id: 'b1', role: 'assistant', content, source: 'hook', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-11-04T00:00:00Z', ...over,
})

function Probe({ m }: { m: Message }) {
  const { button, content } = useMessageTranslation(m)
  return (
    <div>
      <div className="meta">{button}</div>
      <div className="bubble">{content ?? m.content}</div>
    </div>
  )
}

const SETTINGS_ON = { enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true }] }

const EN = 'I fixed the bug in the parser and added a regression test for it.'

test('英文訊息有「翻譯」鈕，中文與 user 訊息沒有', async () => {
  fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  assert.equal(document.querySelector('.msg-translate')?.textContent, '翻譯')
  await unmountAll()
  await mount(<Probe m={msg('已經修好了，請重新整理頁面再試一次。')} />)
  assert.equal(document.querySelector('.msg-translate'), null)
  await unmountAll()
  await mount(<Probe m={msg(EN, { role: 'user' })} />)
  assert.equal(document.querySelector('.msg-translate'), null)
})

test('點了：打 POST /messages/:id/translate，譯文在同一個泡泡裡翻面取代原文，按鈕切回原文', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/messages/m1/translate') ? { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042 } : r.path.endsWith('/claude-api/settings') ? SETTINGS_ON : {}))
  await mount(<Probe m={msg(EN)} />)
  const bubble = () => document.querySelector('.bubble')!
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '譯文顯示')
  assert.ok(reqs.some((r) => r.method === 'POST' && r.path.includes('/messages/m1/translate')))
  assert.match(bubble().textContent ?? '', /我修好了解析器的 bug/)
  assert.doesNotMatch(bubble().textContent ?? '', /parser/, '原文被譯文取代，不是疊在下面')
  assert.match(document.querySelector('.msg-translation-cost')!.textContent ?? '', /haiku-5-5 · 這次翻譯 \$0\.00042/)
  assert.equal(document.querySelector('.msg-translate')?.textContent, '⇄ 原文')
  await click(document.querySelector('.msg-translate')!)
  assert.equal(document.querySelector('.msg-translation'), null)
  assert.match(bubble().textContent ?? '', /parser/)
  assert.equal(document.querySelector('.msg-translate')?.textContent, '⇄ 譯文')
  const before = reqs.length
  await click(document.querySelector('.msg-translate')!)
  assert.ok(document.querySelector('.msg-translation'))
  assert.equal(reqs.length, before, '切換不再打 API')
})

test('未啟用：按鈕事先標「翻譯（未啟用）」，按了只提示、不打翻譯 API', async () => {
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? { ...SETTINGS_ON, uses: [{ id: 'message_translate', enabled: false }] } : {}))
  await mount(<Probe m={msg(EN)} />)
  await until(() => document.querySelector('.msg-translate.off') !== null, '標成未啟用')
  assert.equal(document.querySelector('.msg-translate')?.textContent, '翻譯（未啟用）')
  assert.match(document.querySelector('.msg-translate')?.getAttribute('title') ?? '', /對話訊息翻譯/)
  await click(document.querySelector('.msg-translate')!)
  assert.ok(!reqs.some((r) => r.path.includes('/translate')), '沒有打翻譯 API')
})
