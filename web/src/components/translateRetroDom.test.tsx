import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, settle, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Message } from '../api/types'
import { resetTranslationStore } from '../lib/messageTranslationStore'
import { useMessageTranslation } from './useMessageTranslation'
import { TranslateRecordsPanel } from './TranslateRetroRecords'
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
  id: 'retro-1',
  conversation_id: 'c',
  turn_id: null,
  bot_id: 'b1',
  role: 'assistant',
  content: SRC,
  source: 'hook',
  incomplete: false,
  group_id: null,
  attachments: [],
  relay_from: null,
  terminal_snapshot: null,
  created_at: '2026-11-21T00:00:00Z',
  ...over,
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

test('複盤按鈕在譯文底部直接開翻譯紀錄，不送 POST、沒有備註表單；rejected 也能看紀錄', async () => {
  const base = globalThis.fetch
  const calls: { method: string; url: string }[] = []
  const makeRecord = (status: 'done' | 'rejected') => ({
    at: '2026-11-21T02:00:00Z',
    message_id: 'retro-1',
    bot_id: 'b1',
    project_id: 'p1',
    status,
    model: 'gpt-6-luna',
    effort: 'high',
    prompt_version: '4',
    cost_usd: 0.02,
    attempts: [
      { model: 'claude-haiku-5-5', effort: null, reason: 'untranslated', jev_prob: null, cost_usd: 0.005 },
      { model: 'gpt-6-luna', effort: 'high', reason: status === 'rejected' ? 'untranslated' : null, jev_prob: status === 'rejected' ? null : 0.61, cost_usd: 0.015 },
    ],
    retried: true,
    jev_prob: status === 'rejected' ? null : 0.61,
    low_confidence: false,
    local_checks: { untranslated: status === 'rejected', partially_untranslated: false, markdown_structure: true, japanese_output: false },
    check_failed: true,
    rejected: status === 'rejected' ? 'untranslated' : null,
    reject_detail: null,
    source: SRC,
    translation: '這是譯文',
    notes: [{ at: '2026-11-21T01:00:00Z', note: '舊備註' }],
  })
  let mode: 'done' | 'rejected' = 'done'
  globalThis.fetch = (async (input: string, init?: { method?: string }) => {
    const url = String(input)
    calls.push({ method: init?.method ?? 'GET', url })
    if (url.endsWith('/claude-api/settings')) {
      return new Response(JSON.stringify({ enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: false }] }), { status: 200 })
    }
    if (url.endsWith('/messages/retro-1/translate-record')) {
      return new Response(JSON.stringify({ message_id: 'retro-1', record: makeRecord(mode) }), { status: 200 })
    }
    if (url.includes('/translate') && init?.method === 'POST') {
      if (mode === 'rejected') {
        return new Response(JSON.stringify({ error: 'translate_rejected', reason: 'untranslated', detail: null }), { status: 422 })
      }
      return new Response(JSON.stringify({ translation: '這是譯文', model: 'gpt-6-luna', cached: false, cost_usd: 0.02 }), { status: 200 })
    }
    return new Response(JSON.stringify({}), { status: 200 })
  }) as unknown as typeof fetch

  try {
    await mount(<Probe m={msg()} />)
    assert.equal(document.querySelector('[data-k="translate-retro"]'), null)
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('.msg-translation-foot [data-k="translate-retro"]') !== null, '按鈕在譯文底部費用列')
    assert.equal(document.querySelector('.meta [data-k="translate-retro"]'), null, '不在 meta 列')
    await click(document.querySelector('.msg-translation-foot [data-k="translate-retro"]') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="retro-item"]') !== null, '直接開單則翻譯紀錄')
    assert.equal(document.querySelector('[data-k="translate-retro-form"]'), null, '無備註表單')
    assert.equal(calls.filter((c) => c.url.includes('/translate-report')).length, 0, '不送 POST')
    const modelEl = document.querySelector('[data-k="retro-model"]') as HTMLElement
    assert.equal(modelEl.textContent, 'Luna')
    assert.equal(modelEl.getAttribute('title'), 'gpt-6-luna')
    assert.match(document.querySelector('[data-k="retro-retried"]')?.textContent ?? '', /是（2 次嘗試）/)
    assert.match(document.querySelector('[data-k="retro-note"]')?.textContent ?? '', /舊備註/)

    await unmountAll()
    resetTranslationStore()
    mode = 'rejected'
    await mount(<Probe m={msg()} />)
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-rejected"]') !== null, 'rejected 提示')
    const rejRetro = document.querySelector('[data-k="translate-rejected"] [data-k="translate-retro"]') as HTMLButtonElement
    assert.ok(rejRetro, 'rejected 狀態也有複盤鈕')
    await click(rejRetro)
    await until(() => document.querySelector('[data-k="retro-rejected"]') !== null, '顯示被擋原因')
    assert.match(document.querySelector('[data-k="retro-rejected"]')?.textContent ?? '', /未翻譯/)
  } finally {
    globalThis.fetch = base
  }
})

test('翻譯紀錄子分頁：讀取清單、篩選與載入更多', async () => {
  const base = globalThis.fetch
  const mk = (at: string, id: string, retried = false) => ({
    at,
    message_id: id,
    bot_id: 'b1',
    project_id: 'p1',
    status: 'done',
    model: 'claude-haiku-5-5',
    effort: null,
    prompt_version: '4',
    cost_usd: 0.001,
    attempts: retried
      ? [
          { model: 'claude-haiku-5-5', effort: null, reason: 'untranslated', jev_prob: null, cost_usd: 0.001 },
          { model: 'gpt-6-luna', effort: 'high', reason: null, jev_prob: 0.8, cost_usd: 0.002 },
        ]
      : [],
    retried,
    jev_prob: null,
    low_confidence: false,
    local_checks: null,
    check_failed: false,
    rejected: null,
    reject_detail: null,
    source: 's',
    translation: 't',
    notes: [],
  })
  const urls: string[] = []
  globalThis.fetch = (async (input: string) => {
    const url = String(input)
    urls.push(url)
    if (url.includes('filter=retried')) {
      return new Response(JSON.stringify({ records: [mk('2026-11-21T02:00:00Z', 'm-retry', true)], next_before: null }), { status: 200 })
    }
    if (url.includes('before=')) {
      return new Response(JSON.stringify({ records: [mk('2026-11-20T00:00:00Z', 'm3')], next_before: null }), { status: 200 })
    }
    if (url.includes('/translate-records')) {
      return new Response(
        JSON.stringify({
          records: [mk('2026-11-21T01:00:00Z', 'm1'), mk('2026-11-21T00:30:00Z', 'm2')],
          next_before: '2026-11-21T00:30:00Z',
        }),
        { status: 200 },
      )
    }
    return new Response(JSON.stringify({}), { status: 200 })
  }) as unknown as typeof fetch

  try {
    await mount(<TranslateRecordsPanel active />)
    await until(() => document.querySelectorAll('[data-k="retro-item"]').length === 2, '第一頁兩筆')
    assert.ok(urls.some((u) => /\/translate-records\?limit=50$/.test(u)))
    await click(document.querySelector('[data-k="retro-log-more"]') as HTMLButtonElement)
    await until(() => document.querySelectorAll('[data-k="retro-item"]').length === 3, '載入更多')
    assert.ok(urls.some((u) => u.includes('before=2026-11-21T00%3A30%3A00Z')))
    assert.equal(document.querySelector('[data-k="retro-log-more"]'), null)

    const sel = document.querySelector('[data-k="retro-filter-kind"]') as HTMLSelectElement
    sel.value = 'retried'
    sel.dispatchEvent(new window.Event('change', { bubbles: true }))
    await settle()
    await until(() => document.querySelectorAll('[data-k="retro-item"]').length === 1, '篩選只看重翻過')
    assert.ok(urls.some((u) => u.includes('filter=retried')))
  } finally {
    globalThis.fetch = base
  }
})
