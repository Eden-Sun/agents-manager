import test from 'node:test'
import assert from 'node:assert/strict'
import { balanceView, toClaudeApiBalance } from './claudeApiBalance'
import { balanceTooltipLines } from '../components/balanceTooltip'

const consoleOk = (expires: string | null) => ({
  status: 'ok',
  purchased_usd: 1,
  promotional_usd: 95.99,
  total_usd: 96.99,
  fetched_at: '2026-11-04T10:00:00.000Z',
  promotions: [{ remaining_usd: 95.99, total_usd: 100, expires_on: expires }],
})
const NOW = new Date(2026, 10, 4, 12, 0, 0)
const echo = (k: string, v?: Record<string, string | number>) =>
  Object.entries(v ?? {}).reduce((s, [a, b]) => s.split(`{${a}}`).join(String(b)), k)

test('Console ok 顯示實際值，最近促銷 7 天內到期才警示', () => {
  const far = balanceView(toClaudeApiBalance({ configured: false, console: consoleOk('2026-11-15') }), NOW)
  assert.equal(far.usd, 96.99)
  assert.equal(far.source, 'console')
  assert.equal(far.loggedOut, false)
  assert.equal(far.expiringSoon, false)
  assert.equal(balanceView(toClaudeApiBalance({ configured: false, console: consoleOk('2026-11-11') }), NOW).expiringSoon, true)
  assert.equal(balanceView(toClaudeApiBalance({ configured: false, console: consoleOk('2026-11-12') }), NOW).expiringSoon, false)
})

test('logged_out 退回推算值並提示重新匯出', () => {
  const b = toClaudeApiBalance({
    configured: true,
    anchor: { usd: 100, at: '2026-11-01T00:00:00Z' },
    remaining_usd: 90,
    console: { status: 'logged_out', fetched_at: '2026-11-04T10:00:00.000Z' },
  })
  const v = balanceView(b, NOW)
  assert.equal(v.usd, 90)
  assert.equal(v.source, 'estimate')
  assert.equal(v.loggedOut, true)
  assert.equal(balanceTooltipLines(b, v, echo as never)[0], 'Console 登入已過期，需重新匯出')
})

test('其他失敗退回推算值；兩邊都沒有就不畫', () => {
  const est = toClaudeApiBalance({ configured: true, remaining_usd: 12, console: { status: 'parse_failed' } })
  const v = balanceView(est, NOW)
  assert.equal(v.usd, 12)
  assert.equal(v.source, 'estimate')
  assert.equal(v.loggedOut, false)
  assert.equal(balanceView(toClaudeApiBalance({ configured: false, console: { status: 'not_configured' } }), NOW).usd, undefined)
  assert.equal(balanceView(null, NOW).usd, undefined)
})

test('tooltip 標讀取時間與每筆促銷到期日', () => {
  const b = toClaudeApiBalance({ configured: false, console: consoleOk('2026-11-06') })
  const lines = balanceTooltipLines(b, balanceView(b, NOW), echo as never)
  assert.match(lines[0], /^Console 實際（\d\d:\d\d 讀取）$/)
  assert.ok(lines.some((l) => l.includes('2026-11-06')))
  assert.ok((lines.at(-1) ?? '').includes('⚠'))
})
