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
    assert.equal(roleToExpertSection(null), 'unassigned')
    assert.equal(roleToExpertSection(undefined), 'unassigned')
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
    assert.equal(roleTitle(null), '未分配')
    assert.equal(roleTitle('scheduler'), '排程專用')
  })

  test('groupExpertSections: scheduler bots get their own section before unassigned', () => {
    const secs = groupExpertSections([{ id: 'u', role: null }, { id: 's', role: 'scheduler' }, { id: 'e', role: 'executor' }])
    assert.deepEqual(secs.map((x) => [x.role, x.title, x.items.map((i) => i.id)]), [['executor', '執行者', ['e']], ['scheduler', '排程專用', ['s']], ['unassigned', '未分配', ['u']]])
  })

  test('groupExpertSections: fixed order (manager -> specialist -> advisor -> executor -> unassigned)', () => {
    const bots: (Pick<Bot, 'id'> & Partial<Bot>)[] = [
      { id: 'b1', name: 'bot-exec-1', role: 'executor' },
      { id: 'b2', name: 'bot-unassigned', role: null },
      { id: 'b3', name: 'bot-mgr', role: 'manager' },
      { id: 'b4', name: 'bot-adv', role: 'advisor' },
      { id: 'b5', name: 'bot-exec-2', role: 'executor' },
      { id: 'b6', name: 'bot-spec', role: 'specialist' },
    ]

    const sections = groupExpertSections(bots)
    assert.deepEqual(sections.map((s) => s.role), ['manager', 'specialist', 'advisor', 'executor', 'unassigned'])
    assert.deepEqual(sections[0].items.map((b) => b.id), ['b3'])
    assert.deepEqual(sections[1].items.map((b) => b.id), ['b6'])
    assert.deepEqual(sections[2].items.map((b) => b.id), ['b4'])
    assert.deepEqual(sections[3].items.map((b) => b.id), ['b1', 'b5'])
    assert.deepEqual(sections[4].items.map((b) => b.id), ['b2'])
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
