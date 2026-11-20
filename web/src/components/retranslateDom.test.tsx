/**
 * 「重新翻譯」（2026-11-20）：譯文狀態才出現；按下只打一次 `?retranslate=true`，成功換成新譯文，
 * 被擋（422）就保留舊譯文。只用假的 fetch，不打真 API。
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
  id: 'redo-1', conversation_id: 'c', turn_id: null, bot_id: 'b1', role: 'assistant', content: SRC, source: 'hook', incomplete: false,
  group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '2026-11-20T00:00:00Z', ...over,
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

/** 第一次手動翻回「舊譯文」；重新翻譯（`retranslate=true`）回 `redoReply`（422 時回被擋）。 */
function stubFetch(redoReply: { status: number; body: Record<string, unknown> }, redoCalls: string[]) {
  const base = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: { method?: string }) => {
    const url = String(input)
    if (url.endsWith('/claude-api/settings')) {
      return new Response(JSON.stringify({ enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: false }] }), { status: 200 })
    }
    if (url.includes('/translate') && init?.method === 'POST') {
      if (url.includes('retranslate=true')) {
        redoCalls.push(url)
        return new Response(JSON.stringify(redoReply.body), { status: redoReply.status })
      }
      return new Response(JSON.stringify({ translation: '舊譯文', model: 'claude-haiku-5-5', cached: false, cost_usd: 0.01 }), { status: 200 })
    }
    return new Response(JSON.stringify({}), { status: 200 })
  }) as unknown as typeof fetch
  return () => { globalThis.fetch = base }
}

test('重新翻譯：譯文狀態才出現，按下只打一次，成功換成新譯文', async () => {
  const redoCalls: string[] = []
  const restore = stubFetch({ status: 200, body: { translation: '新譯文', model: 'claude-sonnet-5-5', cached: false, cost_usd: 0.05 } }, redoCalls)
  try {
    await mount(<Probe m={msg()} />)
    assert.equal(document.querySelector('[data-k="translate-redo"]'), null, '未翻譯時不出現')
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('.bubble')?.textContent?.includes('舊譯文') === true, '舊譯文顯示')
    const redo = document.querySelector('[data-k="translate-redo"]') as HTMLButtonElement | null
    assert.ok(redo, '譯文狀態出現「重新翻譯」')
    await click(redo!)
    await until(() => document.querySelector('.bubble')?.textContent?.includes('新譯文') === true, '換成新譯文')
    await settle()
    assert.equal(redoCalls.length, 1, '只打一次 retranslate=true')
  } finally {
    restore()
  }
})

test('重新翻譯沒過（422）：保留舊譯文，不換掉', async () => {
  const redoCalls: string[] = []
  const restore = stubFetch({ status: 422, body: { error: 'translate_rejected', reason: 'partially_untranslated' } }, redoCalls)
  try {
    await mount(<Probe m={msg({ id: 'redo-2' })} />)
    await click(document.querySelector('.msg-translate') as HTMLButtonElement)
    await until(() => document.querySelector('.bubble')?.textContent?.includes('舊譯文') === true, '舊譯文顯示')
    await click(document.querySelector('[data-k="translate-redo"]') as HTMLButtonElement)
    await until(() => redoCalls.length === 1, '重翻已送出')
    await settle()
    assert.ok(document.querySelector('.bubble')?.textContent?.includes('舊譯文'), '沒過時舊譯文還在')
    assert.ok(document.querySelector('[data-k="translate-redo"]'), '按鈕仍在，可以再試')
  } finally {
    restore()
  }
})
