import test, { describe } from 'node:test'
import assert from 'node:assert/strict'
import type { Bot } from '../api/types'
import {
  canReorderInExpertMode,
  groupExpertSections,
  roleShortLabel,
  roleTitle,
  roleToExpertSection,
} from './expertSections'

describe('expertSections pure functions', () => {
  test('roleToExpertSection correctly buckets roles', () => {
    assert.equal(roleToExpertSection('manager'), 'manager')
    assert.equal(roleToExpertSection('advisor'), 'advisor')
    assert.equal(roleToExpertSection('executor'), 'executor')
    assert.equal(roleToExpertSection('specialist'), 'specialist')
    // role 為空不再有「未分配」：被排程綁 → 排程區，否則歸執行者區。
    assert.equal(roleToExpertSection(null), 'executor')
    assert.equal(roleToExpertSection(undefined), 'executor')
    assert.equal(roleToExpertSection(null, true), 'scheduler')
    assert.equal(roleToExpertSection('executor', true), 'executor')
  })

  test('roleShortLabel and roleTitle match specifications', () => {
    assert.equal(roleShortLabel('manager'), '管')
    assert.equal(roleShortLabel('advisor'), '顧')
    assert.equal(roleShortLabel('executor'), '執')
    assert.equal(roleShortLabel('specialist'), '專')
    assert.equal(roleShortLabel(null), '')

    assert.equal(roleTitle('manager'), '管理者')
    assert.equal(roleTitle('advisor'), '顧問')
    assert.equal(roleTitle('executor'), '執行者')
    assert.equal(roleTitle('specialist'), '專家')
    assert.equal(roleTitle(null), '')
    assert.equal(roleTitle('scheduler'), '排程專用')
  })

  test('groupExpertSections: role-less bots join executors, or the scheduler section when a schedule binds them', () => {
    const secs = groupExpertSections([{ id: 'u', role: null }, { id: 's', role: 'scheduler' }, { id: 'e', role: 'executor' }, { id: 'k', role: null, scheduled: true }])
    assert.deepEqual(secs.map((x) => [x.role, x.title, x.items.map((i) => i.id)]), [['executor', '執行者', ['u', 'e']], ['scheduler', '排程專用', ['s', 'k']]])
    assert.ok(!secs.some((x) => x.title === '未分配'))
  })

  test('groupExpertSections: fixed order (manager -> specialist -> advisor -> executor)', () => {
    const bots: (Pick<Bot, 'id'> & Partial<Bot>)[] = [
      { id: 'b1', name: 'bot-exec-1', role: 'executor' },
      { id: 'b2', name: 'bot-unassigned', role: null },
      { id: 'b3', name: 'bot-mgr', role: 'manager' },
      { id: 'b4', name: 'bot-adv', role: 'advisor' },
      { id: 'b5', name: 'bot-exec-2', role: 'executor' },
      { id: 'b6', name: 'bot-spec', role: 'specialist' },
    ]

    const sections = groupExpertSections(bots)
    assert.deepEqual(sections.map((s) => s.role), ['manager', 'specialist', 'advisor', 'executor'])
    assert.deepEqual(sections[0].items.map((b) => b.id), ['b3'])
    assert.deepEqual(sections[1].items.map((b) => b.id), ['b6'])
    assert.deepEqual(sections[2].items.map((b) => b.id), ['b4'])
    assert.deepEqual(sections[3].items.map((b) => b.id), ['b1', 'b2', 'b5'])
  })

  test('groupExpertSections: empty sections are skipped', () => {
    const bots: (Pick<Bot, 'id'> & Partial<Bot>)[] = [
      { id: 'b1', name: 'bot-exec', role: 'executor' },
      { id: 'b2', name: 'bot-mgr', role: 'manager' },
    ]

    const sections = groupExpertSections(bots)
    assert.deepEqual(sections.map((s) => s.role), ['manager', 'executor'])
  })

  test('canReorderInExpertMode: only allows reordering within same role bucket', () => {
    assert.equal(canReorderInExpertMode('manager', 'manager'), true)
    assert.equal(canReorderInExpertMode('executor', 'executor'), true)
    assert.equal(canReorderInExpertMode(null, undefined), true)
    assert.equal(canReorderInExpertMode('manager', 'advisor'), false)
    assert.equal(canReorderInExpertMode('advisor', 'executor'), false)
    assert.equal(canReorderInExpertMode('executor', null), false)
  })
})

test('段落標題帶英文縮寫：Mgr／Spc／Adv／Wkr，未分配沒有（2026-11-07 使用者）', () => {
  const secs = groupExpertSections([{ role: 'manager' }, { role: 'specialist' }, { role: 'advisor' }, { role: 'executor' }, { role: null }] as never[])
  assert.deepEqual(secs.map((s) => s.abbr), ['Mgr', 'Spc', 'Adv', 'Wkr', ''])
})

test('副管理者在管理者後，且不重複出現在原角色段', () => {
  const bots: (Pick<Bot, 'id'> & Partial<Bot>)[] = [
    { id: 'deputy', role: 'advisor' }, { id: 'manager', role: 'manager' },
    { id: 'advisor', role: 'advisor' }, { id: 'specialist', role: 'specialist' },
  ]
  assert.deepEqual(groupExpertSections(bots, 'deputy').map((g) => [g.role, g.items.map((b) => b.id)]), [
    ['manager', ['manager', 'deputy']], ['specialist', ['specialist']], ['advisor', ['advisor']],
  ])
  assert.deepEqual(groupExpertSections(bots, 'missing'), groupExpertSections(bots))
  assert.deepEqual(groupExpertSections(bots, null), groupExpertSections(bots))
  assert.deepEqual(groupExpertSections(bots, 'specialist').map((g) => [g.role, g.items.map((b) => b.id)]), [
    ['manager', ['manager', 'specialist']], ['advisor', ['deputy', 'advisor']],
  ])
  assert.equal(canReorderInExpertMode('manager', 'advisor'), false)
})
