import test from 'node:test'
import assert from 'node:assert/strict'
import { hostBaselineView } from './hostBaseline'
import { toHostBaseline } from '../api/normalize'

const issue = (id: string, severity: 'critical' | 'warn') => ({ id, severity, message: `${id} 不一致` })

test('還沒量過（舊 daemon、沒偵測）→ 未知，不說一致也不說缺', () => {
  const v = hostBaselineView(toHostBaseline(undefined))
  assert.equal(v.level, 'unknown')
  assert.equal(v.text, '一致性：尚未檢查')
  assert.deepEqual(v.items, [])
})

test('探測沒跑完（issues: null）→ 未知，絕不當成全缺', () => {
  const v = hostBaselineView(toHostBaseline({ issues: null, checked_at: '2026-10-02T00:00:00Z', os: 'Linux' }), Date.parse('2026-10-02T01:00:00Z'))
  assert.equal(v.level, 'unknown')
  assert.equal(v.text, '一致性：未知')
  assert.ok(v.hint.length > 0)
  assert.deepEqual(v.items, [])
})

test('一致 → ok', () => {
  const v = hostBaselineView(toHostBaseline({ issues: [], checked_at: 'x', os: 'Linux' }))
  assert.equal(v.level, 'ok')
  assert.equal(v.text, '一致性：與基準一致')
})

test('有嚴重項 → critical，嚴重的排前面，缺什麼逐項列出', () => {
  const v = hostBaselineView(
    toHostBaseline({ issues: [issue('claude.cc2.RTK.md', 'warn'), issue('tool.rtk', 'critical')], checked_at: 'x', os: 'Linux' }),
  )
  assert.equal(v.level, 'critical')
  assert.equal(v.text, '一致性：2 項不一致（1 項嚴重）')
  assert.deepEqual(v.items.map((i) => i.id), ['tool.rtk', 'claude.cc2.RTK.md'])
  assert.equal(v.items[0].message, 'tool.rtk 不一致')
})

test('只有提醒 → warn', () => {
  const v = hostBaselineView(toHostBaseline({ issues: [issue('gitconfig.token', 'warn')], checked_at: 'x', os: null }))
  assert.equal(v.level, 'warn')
  assert.equal(v.text, '一致性：1 項提醒')
})

test('normalize：讀不懂的項目丟掉、severity 不認得的當提醒，issues 不是陣列＝未知', () => {
  const b = toHostBaseline({ issues: [{ id: 'a', severity: 'weird', message: 'm' }, 'junk', { severity: 'critical' }], checked_at: 'x' })
  assert.deepEqual(b?.issues, [{ id: 'a', severity: 'warn', message: 'm' }])
  assert.equal(toHostBaseline({ issues: 'oops', checked_at: 'x' })?.issues, null)
  assert.equal(toHostBaseline(null), null)
})

// ───────── 過期：舊結果不能裝成現在的 ─────────

const NOW = Date.parse('2026-10-02T12:00:00Z')
const ago = (h: number) => new Date(NOW - h * 3600_000).toISOString()

test('daemon 說 stale（最後一次偵測失敗）→ 「上次檢查於…，目前無法連線」，舊的差異仍列出但不再說是現況', () => {
  const b = toHostBaseline({ issues: [issue('tool.rtk', 'critical')], checked_at: ago(1), os: 'Linux', stale: true, failed_at: ago(0), error: 'ssh: connect to host timed out' })
  assert.equal(b?.stale, true)
  assert.equal(b?.error, 'ssh: connect to host timed out')
  const v = hostBaselineView(b, NOW)
  assert.equal(v.level, 'stale')
  assert.ok(v.text.startsWith('一致性：上次檢查於 ') && v.text.endsWith('，目前無法連線'), v.text)
  assert.ok(v.hint.includes('ssh: connect to host timed out'))
  assert.deepEqual(v.items.map((i) => i.id), ['tool.rtk'])
})

test('沒有任何事件、單純太久沒更新（主機一直離線被輪詢跳過）→ 前端自己也標過期', () => {
  const fresh = toHostBaseline({ issues: [], checked_at: ago(6), os: 'Linux' })
  assert.equal(hostBaselineView(fresh, NOW).level, 'ok', '6 小時內不算')
  const old = toHostBaseline({ issues: [], checked_at: ago(8), os: 'Linux' })
  const v = hostBaselineView(old, NOW)
  assert.equal(v.level, 'stale')
  assert.ok(v.text.includes('上次檢查於') && v.text.includes('目前無法連線'))
  assert.ok(v.hint.length > 0)
})

test('過期時「與基準一致」也不能再說一致；時間讀不懂就信 daemon 的 stale 欄位，沒有就不亂標', () => {
  assert.notEqual(hostBaselineView(toHostBaseline({ issues: [], checked_at: ago(30), os: 'Linux' }), NOW).text, '一致性：與基準一致')
  assert.equal(hostBaselineView(toHostBaseline({ issues: [], checked_at: 'x', os: 'Linux' }), NOW).level, 'ok')
  assert.equal(hostBaselineView(toHostBaseline({ issues: [], checked_at: 'x', os: 'Linux', stale: true }), NOW).level, 'stale')
})

test('未知（issues: null）又過期：說上次檢查，而不是只說未知', () => {
  const v = hostBaselineView(toHostBaseline({ issues: null, checked_at: ago(9), os: null, stale: true }), NOW)
  assert.equal(v.level, 'stale')
  assert.deepEqual(v.items, [])
})
