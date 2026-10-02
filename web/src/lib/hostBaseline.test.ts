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
  const v = hostBaselineView(toHostBaseline({ issues: null, checked_at: '2026-10-02T00:00:00Z', os: 'Linux' }))
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
