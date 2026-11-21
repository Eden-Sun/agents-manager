import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, settle, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Message } from '../api/types'
import { resetTranslationStore } from '../lib/messageTranslationStore'
import { useMessageTranslation } from './useMessageTranslation'
import { TranslateRecordCard, TranslateRecordsPanel } from './TranslateRetroRecords'
import type { TranslateRecord } from '../api/translateRetro'
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

const baseRecord = (over: Partial<TranslateRecord> = {}): TranslateRecord => ({
  at: '2026-11-21T06:40:00.000Z', messageId: 'm-att', botId: 'b1', projectId: 'p1', status: 'done', model: 'gpt-6-luna', effort: 'high',
  promptVersion: '4', costUsd: 0.02, attempts: [], retried: true, jevProb: 0.61, lowConfidence: false,
  localChecks: { untranslated: false, partiallyUntranslated: false, markdownStructure: false, japaneseOutput: false }, checkFailed: false,
  rejected: null, rejectDetail: null, source: 'Round 22: add tests.', translation: '第 22 輪：補測試。', notes: [], ...over,
})

const failedAttempt = (over: Record<string, unknown> = {}) => ({
  model: 'claude-haiku-5-5', effort: null, reason: 'untranslated', jevProb: null, costUsd: 0.004,
  text: null, textTruncated: false, hasText: true, sameAsFinal: false,
  checks: { untranslated: true, partiallyUntranslated: false, markdownStructure: false, japaneseOutput: false }, ...over,
})

test('嘗試可展開看那一版譯文：未過的標示、列出未過原因，並與原文對照', async () => {
  const record = baseRecord({
    attempts: [
      { ...failedAttempt({ text: 'Round 22: add tests.', hasText: true }), text: 'Round 22: add tests.' },
      { model: 'gpt-6-luna', effort: 'high', reason: null, jevProb: 0.61, costUsd: 0.015, text: null, textTruncated: false, hasText: false, sameAsFinal: true, checks: null },
    ] as TranslateRecord['attempts'],
  })
  await mount(<TranslateRecordCard r={record} bots={[]} />)
  const rows = document.querySelectorAll<HTMLElement>('[data-k="attempt-row"]')
  assert.equal(rows.length, 2)
  assert.ok(rows[0].classList.contains('failed'), '未過的那一列有標示')
  assert.ok(document.querySelector('[data-k="attempt-failed"]'), '未過原因在摘要裡')
  await click(rows[0].querySelector('summary') as HTMLElement)
  await settle()
  const version = rows[0].querySelector('[data-k="attempt-version"]') as HTMLElement
  assert.ok(version, '展開後看到這一版譯文')
  assert.equal(version.querySelector('pre')?.textContent, 'Round 22: add tests.')
  assert.ok(version.classList.contains('failed'), '譯文區塊醒目標示')
  assert.match(rows[0].querySelector('[data-k="attempt-reasons"]')?.textContent ?? '', /未翻譯/, '列出未過的檢查')
  assert.equal(rows[0].querySelectorAll('.retro-pair pre')[0].textContent, 'Round 22: add tests.', '原文在旁邊對照')
})

test('清單的嘗試只帶旗標：展開時才去單則讀全文；最終採用的那次顯示最終譯文', async () => {
  const urls: string[] = []
  const base = globalThis.fetch
  globalThis.fetch = (async (input: string) => {
    const url = String(input)
    urls.push(url)
    if (url.endsWith('/messages/m-att/translate-record')) {
      return new Response(JSON.stringify({ message_id: 'm-att', record: { at: '2026-11-21T06:40:00.000Z', attempts: [{ model: 'claude-haiku-5-5', reason: 'untranslated', text: '完整的未過譯文', has_text: true }] } }), { status: 200 })
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch
  try {
    const record = baseRecord({ attempts: [failedAttempt({ hasText: true, text: null })] })
    await mount(<TranslateRecordCard r={record} bots={[]} />)
    const row = document.querySelector<HTMLElement>('[data-k="attempt-row"]')!
    await click(row.querySelector('summary') as HTMLElement)
    await until(() => row.querySelector('[data-k="attempt-version"] pre')?.textContent === '完整的未過譯文', '展開後讀到全文')
    assert.ok(urls.some((u) => u.endsWith('/messages/m-att/translate-record')), '展開時才讀單則')
  } finally {
    globalThis.fetch = base
  }
})
