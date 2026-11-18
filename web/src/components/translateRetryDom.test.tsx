/**
 * 品質檢查沒過的細節與「回報給管理者」（使用者 2026-11-18）：提示列可點開看原因摘要；按鈕呼叫 daemon 回報端點，
 * 重複（10 分鐘內）只顯示「不重複送出」。只用假的 fetch，不打真 API。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, settle, teardownDom, unmountAll, until } from '../testing/domHarness'
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
  id: 'rep-1', conversation_id: 'c', turn_id: null, bot_id: 'b1', role: 'assistant', content: SRC, source: 'hook', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-11-18T00:00:00Z', ...over,
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

const detail = { check: 'untranslated', src_latin: 50, src_cjk: 0, reply_latin: 50, reply_cjk: 0, identical: true, models: ['claude-haiku-5-5', 'claude-sonnet-5-5'], attempts: 2 }

/** 翻譯回 422（附細節）；報告端點第一次回 sent:true，之後回 sent:false（重複）。 */
function stubFetch(reports: string[]) {
  const base = globalThis.fetch
  let reported = 0
  globalThis.fetch = (async (input: string, init?: { method?: string }) => {
    const url = String(input)
    if (url.includes('/translate-report')) {
      reports.push(url)
      const sent = reported++ === 0
      return new Response(JSON.stringify(sent ? { sent: true, manager_bot_id: 'mgr' } : { sent: false, reason: 'duplicate' }), { status: 200 })
    }
    if (url.endsWith('/claude-api/settings')) {
      return new Response(JSON.stringify({ enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: false }] }), { status: 200 })
    }
    if (url.includes('/translate') && init?.method === 'POST') {
      return new Response(JSON.stringify({ error: 'translate_rejected', reason: 'untranslated', detail }), { status: 422 })
    }
    return new Response(JSON.stringify({}), { status: 200 })
  }) as unknown as typeof fetch
  return () => { globalThis.fetch = base }
}

test('品質沒過：點開「原因」看得到檢查、字數、是否相同、模型與次數', async () => {
  const restore = stubFetch([])
  try {
    await mount(<Probe m={msg()} />)
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-reject-detail"]') !== null, '顯示原因')
    const text = document.querySelector('[data-k="translate-reject-detail"]')!.textContent ?? ''
    assert.match(text, /untranslated/)
    assert.match(text, /claude-haiku-5-5 → claude-sonnet-5-5/)
    assert.match(text, /2 次/)
    assert.match(text, /是/, '譯文與原文相同：是')
  } finally {
    restore()
  }
})

test('回報給管理者：第一次送出；10 分鐘內再按只顯示「不重複送出」', async () => {
  const reports: string[] = []
  const restore = stubFetch(reports)
  try {
    await mount(<Probe m={msg()} />)
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('[data-k="translate-report"]') !== null, '有回報按鈕')
    await click(document.querySelector('[data-k="translate-report"]') as HTMLButtonElement)
    await settle()
    assert.equal(reports.length, 1)
    assert.match(reports[0], /\/messages\/rep-1\/translate-report$/)
    await click(document.querySelector('[data-k="translate-report"]') as HTMLButtonElement)
    await settle()
    assert.equal(reports.length, 2, '第二次照樣呼叫端點，由 daemon 判斷重複')
  } finally {
    restore()
  }
})
