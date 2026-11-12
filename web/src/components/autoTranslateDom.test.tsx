/**
 * 自動翻譯英文訊息（使用者 2026-11-06：「當判斷是英文訊息時自動用 API 翻譯，我就不用手動按」）。
 * daemon 設定 `message_translate.auto` 開著，最後 N 則／新到的、已完整的英文訊息掛載後自動翻成譯文；
 * 手動點回原文的不再自動翻；串流中的不翻；失敗不重試轟炸。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Message } from '../api/types'
import { AUTO_FAIL_LIMIT, resetTranslationStore } from '../lib/messageTranslationStore'
import { AUTO_TRANSLATE_RECENT, useMessageTranslation } from './useMessageTranslation'
import { resetTranslateReady } from './useTranslateReady'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetTranslateReady()
  resetTranslationStore()
  localStorage.removeItem('am.msgTranslate.want')
  localStorage.removeItem('am.msgTranslate.declined')
})
after(teardownDom)

const msg = (content: string, over: Partial<Message> = {}): Message => ({
  id: 'm1', conversation_id: 'c', turn_id: null, bot_id: 'b1', role: 'assistant', content, source: 'hook', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-11-06T00:00:00Z', ...over,
})

function Probe({ m, recent = true }: { m: Message; recent?: boolean }) {
  const { button, content } = useMessageTranslation(m, { recent })
  return (
    <div className={`row-${m.id}`}>
      <div className="meta">{button}</div>
      <div className="bubble">{content ?? m.content}</div>
    </div>
  )
}

const settings = (over: Record<string, unknown> = {}) => ({
  enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: true, ...over }],
})
const EN = 'I fixed the bug in the parser and added a regression test for it.'
const TR = { translation: '我修好了解析器的 bug。', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.00042 }
const route = (s: object = settings()) => (r: { path: string }) => (r.path.includes('/translate') ? TR : r.path.endsWith('/claude-api/settings') ? s : {})
const translateCalls = (reqs: { method: string; path: string }[]) => reqs.filter((r) => r.method === 'POST' && r.path.includes('/translate')).length

test('auto 開、最後 N 則內、已完整的英文訊息：不用按就自動翻出譯文與費用行', async () => {
  const reqs = fakeApi(route())
  await mount(<Probe m={msg(EN)} />)
  await until(() => document.querySelector('.msg-translation') !== null, '自動翻出譯文')
  assert.equal(translateCalls(reqs), 1)
  assert.match(document.querySelector('.bubble')!.textContent ?? '', /我修好了解析器的 bug/)
  assert.equal(document.querySelector('.msg-translation-cost')!.textContent, '(by Haiku $0.00042)')
  assert.equal(document.querySelector('.msg-translate')?.textContent, '⇄ 原文')
  await settle(30)
  assert.equal(translateCalls(reqs), 1, '同一則只打一次')
  // 自動翻出來的跟手動點過一樣記著（之後滑出最後 N 則、重整都維持譯文）。
  assert.deepEqual(JSON.parse(localStorage.getItem('am.msgTranslate.want') ?? '[]'), ['m1'])
})

test('手動點回原文：記住（declined），remount 與重整後都不再自動翻回去；再點才翻', async () => {
  const reqs = fakeApi(route())
  await mount(<Probe m={msg(EN)} />)
  await until(() => document.querySelector('.msg-translation') !== null, '自動翻出譯文')
  await click(document.querySelector('.msg-translate')!)
  assert.equal(document.querySelector('.msg-translation'), null)
  assert.deepEqual(JSON.parse(localStorage.getItem('am.msgTranslate.declined') ?? '[]'), ['m1'])
  await unmountAll()
  await mount(<Probe m={msg(EN)} />)
  await settle(60)
  assert.equal(document.querySelector('.msg-translation'), null, 'remount 後仍是原文')
  // 模擬重整：記憶體狀態清空，只剩 localStorage。
  await unmountAll()
  resetTranslationStore()
  resetTranslateReady()
  await mount(<Probe m={msg(EN)} />)
  await settle(60)
  assert.equal(document.querySelector('.msg-translation'), null, '重整後也不再自動翻')
  assert.equal(document.querySelector('.msg-translate')?.textContent, '翻譯')
  assert.equal(translateCalls(reqs), 1)
  // 再點才翻，並解除 declined。
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '手動再翻')
  assert.deepEqual(JSON.parse(localStorage.getItem('am.msgTranslate.declined') ?? '[]'), [])
})

test('daemon 設定 auto 關：不自動翻，按鈕照舊可手動', async () => {
  const reqs = fakeApi(route(settings({ auto: false })))
  await mount(<Probe m={msg(EN)} />)
  await settle(80)
  assert.equal(translateCalls(reqs), 0)
  assert.equal(document.querySelector('.msg-translate')?.textContent, '翻譯')
  await click(document.querySelector('.msg-translate')!)
  await until(() => document.querySelector('.msg-translation') !== null, '手動翻')
})

test('舊 daemon 沒有 auto 欄位、用途未啟用、設定讀不到：都不自動翻', async () => {
  for (const s of [
    { ...settings(), uses: [{ id: 'message_translate', enabled: true }] },
    settings({ enabled: false }),
    { ...settings(), key_present: false },
    { ...settings(), enabled: false },
  ]) {
    const reqs = fakeApi(route(s))
    await mount(<Probe m={msg(EN)} />)
    await settle(60)
    assert.equal(translateCalls(reqs), 0)
    await unmountAll()
    resetTranslateReady()
    resetTranslationStore()
  }
  const reqs = fakeApi((r) => {
    if (r.path.endsWith('/claude-api/settings')) throw new Error('down')
    return TR
  })
  await mount(<Probe m={msg(EN)} />)
  await settle(60)
  assert.equal(translateCalls(reqs), 0, '讀不到設定＝不知道會不會花錢＝不動')
})

test('串流中（incomplete）不翻；完成後才翻', async () => {
  const reqs = fakeApi(route())
  await mount(<Probe m={msg(EN, { incomplete: true })} />)
  await settle(80)
  assert.equal(translateCalls(reqs), 0, '半截內容不打 API')
  await unmountAll()
  await mount(<Probe m={msg(EN, { incomplete: false })} />)
  await until(() => document.querySelector('.msg-translation') !== null, '完成後自動翻')
  assert.equal(translateCalls(reqs), 1)
})

test('不在最後 N 則內（recent=false）不自動翻，也不是英文／不符 eligible 的不翻', async () => {
  const reqs = fakeApi(route())
  await mount(<Probe m={msg(EN)} recent={false} />)
  await settle(60)
  assert.equal(translateCalls(reqs), 0)
  await unmountAll()
  await mount(<Probe m={msg('已經修好了，請重新整理頁面再試一次。')} />)
  await mount(<Probe m={msg(EN, { id: 'm2', source: 'terminal_fallback' })} />)
  await mount(<Probe m={msg(EN, { id: 'm3', rewound_at: '2026-11-06T01:00:00Z' })} />)
  await mount(<Probe m={msg(EN, { id: 'm4', role: 'user' })} />)
  await settle(60)
  assert.equal(translateCalls(reqs), 0)
})

test('一串訊息只有最後 N 則自動翻', async () => {
  const reqs = fakeApi(route())
  const all = Array.from({ length: AUTO_TRANSLATE_RECENT + 4 }, (_, i) => msg(EN, { id: `n${i}` }))
  const recentIds = new Set(all.slice(-AUTO_TRANSLATE_RECENT).map((m) => m.id))
  await mount(
    <div>
      {all.map((m) => (
        <Probe key={m.id} m={m} recent={recentIds.has(m.id)} />
      ))}
    </div>,
  )
  await until(() => translateCalls(reqs) === AUTO_TRANSLATE_RECENT, '最後 N 則都翻了')
  await settle(60)
  assert.equal(translateCalls(reqs), AUTO_TRANSLATE_RECENT)
  const posted = new Set(reqs.filter((r) => r.method === 'POST' && r.path.includes('/translate')).map((r) => r.path))
  assert.ok(!posted.has('/api/messages/n0/translate') && ![...posted].some((p) => p.includes('/n3/')), '更早的不回填')
})

test('自動翻失敗：不跳錯誤、不重試，停在可手動重試的「翻譯」；連續失敗到上限後本頁不再自動翻新的', async () => {
  const reqs = fakeApi((r) => {
    if (r.path.endsWith('/claude-api/settings')) return settings()
    if (r.path.includes('/translate')) throw new Error('claude_api_cap')
    return {}
  })
  await mount(<Probe m={msg(EN)} />)
  await until(() => translateCalls(reqs) === 1 && document.querySelector('.msg-translate')?.textContent === '翻譯', '失敗後回到「翻譯」')
  await settle(60)
  assert.equal(translateCalls(reqs), 1, '不重試')
  await unmountAll()
  await mount(<Probe m={msg(EN)} />)
  await settle(60)
  assert.equal(translateCalls(reqs), 1, 'remount 也不重試')
  // 累積到上限：之後新的訊息不再自動打。
  for (let i = 0; i < AUTO_FAIL_LIMIT; i++) {
    await unmountAll()
    await mount(<Probe m={msg(EN, { id: `f${i}` })} />)
    await settle(40)
  }
  const before = translateCalls(reqs)
  await unmountAll()
  await mount(<Probe m={msg(EN, { id: 'after-limit' })} />)
  await settle(60)
  assert.equal(translateCalls(reqs), before, '到上限後不再自動')
  assert.equal(document.querySelector('.msg-translate')?.textContent, '翻譯', '仍可手動')
})
