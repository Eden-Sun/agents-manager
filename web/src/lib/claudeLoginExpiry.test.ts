import test from 'node:test'
import assert from 'node:assert/strict'
import {
  ONE_DAY_MS,
  THREE_DAYS_MS,
  claudeExpiryState,
  collectClaudeExpiryAlerts,
  expiryNoticeText,
  formatExpiryTime,
} from './claudeLoginExpiry.ts'

const now = Date.UTC(2026, 10, 22, 0, 0, 0)

test('claudeExpiryState returns null for missing, invalid, or >= 3 days remaining', () => {
  assert.equal(claudeExpiryState(undefined, now), null)
  assert.equal(claudeExpiryState(null, now), null)
  assert.equal(claudeExpiryState(0, now), null)
  assert.equal(claudeExpiryState(-1, now), null)
  assert.equal(claudeExpiryState(now + THREE_DAYS_MS, now), null)
  assert.equal(claudeExpiryState(now + THREE_DAYS_MS + 60_000, now), null)
})

test('claudeExpiryState returns warn under 3 days and expired at or past 0', () => {
  assert.equal(claudeExpiryState(now + THREE_DAYS_MS - 1, now)?.stage, 'warn')
  assert.equal(claudeExpiryState(now + ONE_DAY_MS - 1, now)?.stage, 'warn')
  assert.equal(claudeExpiryState(now, now)?.stage, 'expired')
  assert.equal(claudeExpiryState(now - 10_000, now)?.stage, 'expired')
})

test('formatExpiryTime and expiryNoticeText format UTC+8', () => {
  const exp = Date.UTC(2026, 10, 22, 16, 0, 0)
  assert.equal(formatExpiryTime(exp, 8), '11/23 00:00 (+8)')
  assert.equal(
    expiryNoticeText({ host: 'local', account: 'cc0', expiresAtMs: exp, stage: 'warn' }, '本機', 8),
    'cc0（本機）的 Claude 登入將於 11/23 00:00 (+8) 到期',
  )
  assert.equal(
    expiryNoticeText({ host: 'local', account: 'cc0', expiresAtMs: exp, stage: 'expired' }, '本機', 8),
    'cc0（本機）的 Claude 登入已過期',
  )
})

test('collectClaudeExpiryAlerts collects alerts across local and remote hosts sorted by account', () => {
  const alerts = collectClaudeExpiryAlerts({
    localExpiry: { cc1: now + 10_000_000, cc0: now - 1000 },
    hosts: [
      { name: 'local', claude_login_expires_at: { cc0: now - 1000 } },
      { name: 'm1', claude_login_expires_at: { cc2: now + 3_600_000, ok: now + THREE_DAYS_MS + 1000 } },
    ],
    nowMs: now,
  })
  assert.deepEqual(
    alerts.map((a) => `${a.host}:${a.account}:${a.stage}`),
    ['local:cc0:expired', 'local:cc1:warn', 'm1:cc2:warn'],
  )
})
