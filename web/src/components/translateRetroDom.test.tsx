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
import { RetroRecordModal } from './TranslateRetroRecords'
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

test('複盤：展開輸入框、送出打一次端點並帶使用者那句話；送出後顯示已記錄與查看，同一句備註不重複存', async () => {
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
    assert.match(document.querySelector('[data-k="translate-retro-sent"]')!.textContent ?? '', /已記錄複盤/)
    assert.ok(document.querySelector('[data-k="translate-retro-view"]'), '送出後有查看')
    assert.equal(document.querySelector('[data-k="translate-retro-form"]'), null, '送出後輸入框收起')

    // 同一句備註再送一次：daemon 回 duplicate，不重複存（UI 仍顯示已記錄的時間）。
    await click(document.querySelector('[data-k="translate-retro"]') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-retro-form"]') !== null, '再次展開輸入框')
    await typeInto(document.querySelector('.msg-translate-retro-input') as HTMLInputElement, '專有名詞沒保留')
    await click(document.querySelector('[data-k="translate-retro-send"]') as HTMLButtonElement)
    await settle()
    assert.equal(reports.length, 2, '第二次照樣打端點（由 daemon 去重）')
    assert.match(document.querySelector('[data-k="translate-retro-sent"]')?.textContent ?? '', /已記錄複盤/, '仍顯示已記錄的時間')
  } finally {
    restore()
  }
})

test('複盤「查看」打開這則訊息的複盤記錄（只讀，新到舊，原文／譯文可點開）', async () => {
  const base = globalThis.fetch
  const retro = (at: string, note: string | null) => ({
    at, message_id: 'retro-1', bot_id: 'b1', project_id: 'p1', note, model: 'gpt-6-luna', effort: 'high', prompt_version: '4',
    cost_usd: 0.02, jev_prob: 0.61, low_confidence: false,
    attempts: [{ model: 'claude-haiku-5-5', effort: null, reason: 'untranslated', jev_prob: null, cost_usd: 0.005 }, { model: 'gpt-6-luna', effort: 'high', reason: null, jev_prob: 0.61, cost_usd: 0.015 }],
    local_checks: { untranslated: false, partially_untranslated: false, markdown_structure: true, japanese_output: false },
    source: 'The source text', translation: '這是譯文',
  })
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    const url = String(input)
    if (url.includes('/translate-report')) return new Response(JSON.stringify({ sent: true, sent_at: '2026-11-21T00:00:00Z' }), { status: 200 })
    if (url.endsWith('/claude-api/settings')) {
      return new Response(JSON.stringify({ enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: false }] }), { status: 200 })
    }
    if (url.endsWith('/messages/retro-1/translate-retros')) {
      return new Response(JSON.stringify({ message_id: 'retro-1', retros: [retro('2026-11-21T01:00:00Z', null), retro('2026-11-21T02:00:00Z', '專有名詞沒保留')] }), { status: 200 })
    }
    if (url.includes('/translate') && init?.method === 'POST') {
      return new Response(JSON.stringify({ translation: '這是譯文', model: 'gpt-6-luna', cached: false, cost_usd: 0.02 }), { status: 200 })
    }
    return new Response(JSON.stringify({}), { status: 200 })
  }) as unknown as typeof fetch
  try {
    await mount(<Probe m={msg()} />)
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-retro"]') !== null, '有複盤按鈕')
    await click(document.querySelector('[data-k="translate-retro"]') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-retro-form"]') !== null, '展開輸入框')
    await click(document.querySelector('[data-k="translate-retro-send"]') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-retro-view"]') !== null, '送出後有查看')
    await click(document.querySelector('[data-k="translate-retro-view"]') as HTMLButtonElement)
    await until(() => document.querySelectorAll('[data-k="retro-item"]').length === 2, '查看列出兩筆')
    const items = [...document.querySelectorAll('[data-k="retro-item"]')]
    assert.match(items[0].querySelector('[data-k="retro-note"]')?.textContent ?? '', /專有名詞沒保留/, '新到舊：第一筆是有備註那筆')
    assert.equal(items[0].querySelector('[data-k="retro-model"]')?.textContent, 'Luna', '模型用既有顯示名')
    assert.match(items[0].querySelector('[data-k="retro-retried"]')?.textContent ?? '', /是（2 次嘗試）/)
    assert.equal(items[0].querySelectorAll('[data-k="retro-attempts"] li').length, 2, '每次嘗試都列出')
    assert.match(items[0].querySelector('[data-k="retro-checks"]')?.textContent ?? '', /Markdown/, '檢查結果有列出未過項目')
    assert.equal(items[0].querySelector('details')?.hasAttribute('open'), false, '原文／譯文預設收起')
  } finally {
    globalThis.fetch = base
  }
})

test('環境設定的「複盤記錄」清單：讀第一頁、可載入更多（游標 before）', async () => {
  const base = globalThis.fetch
  const retro = (at: string, msgId: string) => ({
    at, message_id: msgId, bot_id: 'b1', project_id: 'p1', note: null, model: 'claude-haiku-5-5', effort: null, prompt_version: '4',
    cost_usd: null, jev_prob: null, low_confidence: false, attempts: [], local_checks: { untranslated: false, partially_untranslated: false, markdown_structure: false, japanese_output: false },
    source: 's', translation: 't',
  })
  const urls: string[] = []
  globalThis.fetch = (async (input: string) => {
    const url = String(input)
    urls.push(url)
    if (url.includes('/translate-retros') && url.includes('before=')) {
      return new Response(JSON.stringify({ retros: [retro('2026-11-20T00:00:00Z', 'm3')], next_before: null }), { status: 200 })
    }
    if (url.includes('/translate-retros')) {
      return new Response(JSON.stringify({ retros: [retro('2026-11-21T01:00:00Z', 'm1'), retro('2026-11-21T00:30:00Z', 'm2')], next_before: '2026-11-21T00:30:00Z' }), { status: 200 })
    }
    return new Response(JSON.stringify({}), { status: 200 })
  }) as unknown as typeof fetch
  try {
    await mount(<RetroRecordModal open onClose={() => {}} />)
    await until(() => document.querySelectorAll('[data-k="retro-item"]').length === 2, '第一頁兩筆')
    assert.ok(urls.some((u) => /\/translate-retros\?limit=30$/.test(u)), '第一頁帶 limit')
    await click(document.querySelector('[data-k="retro-log-more"]') as HTMLButtonElement)
    await until(() => document.querySelectorAll('[data-k="retro-item"]').length === 3, '載入更多後三筆')
    assert.ok(urls.some((u) => u.includes('before=2026-11-21T00%3A30%3A00Z')), '游標原樣帶回')
    assert.equal(document.querySelector('[data-k="retro-log-more"]'), null, '最後一頁沒有載入更多')
  } finally {
    globalThis.fetch = base
  }
})
