/**
 * 「複盤」（使用者 2026-11-21）：譯文已完成才出現；按下展開小輸入框，送出打一次回報端點並帶上使用者那句話；
 * 送出後顯示已開的時間，10 分鐘內再按只顯示時間、不重送。品質沒過（422）時不出現。只用假的 fetch，不打真 API。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, settle, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import type { Message } from '../api/types'
import { resetTranslationStore } from '../lib/messageTranslationStore'
import { useMessageTranslation } from './useMessageTranslation'
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

const SRC = 'The system has encountered an unexpected condition.'
const msg = (over: Partial<Message> = {}): Message => ({
  id: 'retro-1', conversation_id: 'c', turn_id: null, bot_id: 'b1', role: 'assistant', content: SRC, source: 'hook', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-11-21T00:00:00Z', ...over,
})

function Probe({ m }: { m: Message }) {
  const { button, content } = useMessageTranslation(m, { recent: false })
  return (
    <div className="row">
      <div className="meta">{button}</div>
      <div className="bubble">{content ?? m.content}</div>
    </div>
  )
}

/** 翻譯回成功（譯文狀態）或 422（品質沒過）；回報端點記下每次呼叫的 body，第一次 sent:true 並給 sent_at，之後 sent:false（重複）。 */
function stubFetch(opts: { rejected?: boolean; reports: { url: string; body: unknown }[] }) {
  const base = globalThis.fetch
  let reported = 0
  const sentAt = new Date().toISOString()
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    const url = String(input)
    if (url.includes('/translate-report')) {
      opts.reports.push({ url, body: init?.body ? JSON.parse(init.body) : undefined })
      const first = reported++ === 0
      return new Response(JSON.stringify(first ? { sent: true, manager_bot_id: 'mgr', sent_at: sentAt } : { sent: false, reason: 'duplicate', sent_at: sentAt }), { status: 200 })
    }
    if (url.endsWith('/claude-api/settings')) {
      return new Response(JSON.stringify({ enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: false }] }), { status: 200 })
    }
    if (url.includes('/translate') && init?.method === 'POST') {
      if (opts.rejected) {
        return new Response(JSON.stringify({ error: 'translate_rejected', reason: 'untranslated', detail: null }), { status: 422 })
      }
      return new Response(JSON.stringify({ translation: '這是譯文', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.01 }), { status: 200 })
    }
    return new Response(JSON.stringify({}), { status: 200 })
  }) as unknown as typeof fetch
  return () => { globalThis.fetch = base }
}

test('複盤按鈕只在譯文狀態出現；品質沒過（422）時沒有', async () => {
  const restore = stubFetch({ reports: [] })
  try {
    await mount(<Probe m={msg()} />)
    assert.equal(document.querySelector('[data-k="translate-retro"]'), null, '還沒翻譯時沒有')
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-retro"]') !== null, '譯文狀態有複盤')
  } finally {
    restore()
  }
  await unmountAll()
  resetTranslationStore()
  const restoreRejected = stubFetch({ rejected: true, reports: [] })
  try {
    await mount(<Probe m={msg({ id: 'retro-2' })} />)
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-rejected"]') !== null, '品質沒過提示')
    assert.equal(document.querySelector('[data-k="translate-retro"]'), null, '品質沒過時沒有複盤')
  } finally {
    restoreRejected()
  }
})

test('複盤：展開輸入框、送出打一次端點並帶使用者那句話；送出後 10 分鐘內再按只顯示時間', async () => {
  const reports: { url: string; body: unknown }[] = []
  const restore = stubFetch({ reports })
  try {
    await mount(<Probe m={msg()} />)
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-retro"]') !== null, '有複盤按鈕')
    assert.equal(document.querySelector('[data-k="translate-retro-form"]'), null, '一開始沒有輸入框')
    await click(document.querySelector('[data-k="translate-retro"]') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-retro-form"]') !== null, '展開輸入框')
    await typeInto(document.querySelector('.msg-translate-retro-input') as HTMLInputElement, '專有名詞沒保留')
    await click(document.querySelector('[data-k="translate-retro-send"]') as HTMLButtonElement)
    await settle()
    assert.equal(reports.length, 1, '送出只打一次')
    assert.match(reports[0].url, /\/messages\/retro-1\/translate-report$/)
    assert.deepEqual(reports[0].body, { note: '專有名詞沒保留' }, '帶使用者那句話')
    await until(() => document.querySelector('[data-k="translate-retro-sent"]') !== null, '送出後顯示已開的時間')
    assert.match(document.querySelector('[data-k="translate-retro-sent"]')!.textContent ?? '', /已開複盤給管理者/)
    assert.equal(document.querySelector('[data-k="translate-retro-form"]'), null, '送出後輸入框收起')

    // 10 分鐘內再按：只切換顯示時間，不再開輸入框、也不重送。
    await click(document.querySelector('[data-k="translate-retro"]') as HTMLButtonElement)
    await settle()
    assert.equal(document.querySelector('[data-k="translate-retro-form"]'), null, '窗內不再開輸入框')
    assert.equal(reports.length, 1, '窗內不重送')
    assert.match(document.querySelector('[data-k="translate-retro-sent"]')?.textContent ?? '', /已開複盤給管理者/, '窗內再按顯示已送出的時間')
  } finally {
    restore()
  }
})
