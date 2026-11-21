const SOURCE = 'Round 22: add tests for this part. Both tests are green now.'
const FINAL = '第 22 輪：為這部分補上測試。兩個測試現在都通過。'

const checks = (over: Record<string, boolean> = {}) => ({ untranslated: false, partially_untranslated: false, markdown_structure: false, japanese_output: false, ...over })

const ATTEMPTS = [
  { model: 'claude-haiku-5-5', effort: null, reason: 'untranslated', jev_prob: null, cost_usd: 0.004, text: SOURCE, text_truncated: false, has_text: true, checks: checks({ untranslated: true }) },
  { model: 'gpt-6-luna', effort: 'high', reason: null, jev_prob: 0.61, cost_usd: 0.015, same_as_final: true, checks: checks() },
]

const RECORD = {
  at: '2026-11-21T06:40:00.000Z',
  message_id: 'mock-tr-1',
  bot_id: 'mock-bot',
  project_id: 'mock-project',
  status: 'done',
  model: 'gpt-6-luna',
  effort: 'high',
  prompt_version: '4',
  cost_usd: 0.019,
  jev_prob: 0.61,
  low_confidence: false,
  attempts: ATTEMPTS,
  retried: true,
  checks: checks(),
  checks_failed: false,
  reject_reason: null,
  reject_detail: null,
  source: SOURCE,
  translation: FINAL,
  notes: [],
}

export function mockTranslateMessage() {
  return { translation: FINAL, model: 'gpt-6-luna', cached: false, cost_usd: 0.019, low_confidence: false, jev_prob: 0.61 }
}

export function mockTranslateRecord(messageId: string) {
  return { message_id: messageId, record: { ...RECORD, message_id: messageId } }
}

export function mockTranslateRecords(q: URLSearchParams) {
  const limit = Math.min(Number(q.get('limit') ?? '50'), 200)
  const rows = [RECORD, { ...RECORD, at: '2026-11-21T05:10:00.000Z', message_id: 'mock-tr-2', model: 'claude-haiku-5-5', attempts: [], retried: null, cost_usd: 0.003, translation: '第 21 輪：完成。', source: 'Round 21: done.' }]
  const page = rows.slice(0, limit).map((r) => ({
    ...r,
    attempts: r.attempts.map(({ text: _text, ...rest }) => ({ ...rest, has_text: rest.has_text === true })),
  }))
  return { records: page, next_before: null }
}
