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
    assert.equal(roleToExpertSection(null), 'unassigned')
    assert.equal(roleToExpertSection(undefined), 'unassigned')
  })

  test('roleShortLabel and roleTitle match specifications', () => {
    assert.equal(roleShortLabel('manager'), '管')
    assert.equal(roleShortLabel('advisor'), '顧')
    assert.equal(roleShortLabel('executor'), '執')
    assert.equal(roleShortLabel(null), '')

    assert.equal(roleTitle('manager'), '管理者')
    assert.equal(roleTitle('advisor'), '顧問')
    assert.equal(roleTitle('executor'), '執行者')
    assert.equal(roleTitle(null), '未分配')
  })

  test('groupExpertSections: fixed order (manager -> advisor -> executor -> unassigned)', () => {
    const bots: Partial<Bot>[] = [
      { id: 'b1', name: 'bot-exec-1', role: 'executor' },
      { id: 'b2', name: 'bot-unassigned', role: null },
      { id: 'b3', name: 'bot-mgr', role: 'manager' },
      { id: 'b4', name: 'bot-adv', role: 'advisor' },
      { id: 'b5', name: 'bot-exec-2', role: 'executor' },
    ]

    const sections = groupExpertSections(bots)
    assert.deepEqual(sections.map((s) => s.role), ['manager', 'advisor', 'executor', 'unassigned'])
    assert.deepEqual(sections[0].items.map((b) => b.id), ['b3'])
    assert.deepEqual(sections[1].items.map((b) => b.id), ['b4'])
    assert.deepEqual(sections[2].items.map((b) => b.id), ['b1', 'b5'])
    assert.deepEqual(sections[3].items.map((b) => b.id), ['b2'])
  })

  test('groupExpertSections: empty sections are skipped', () => {
    const bots: Partial<Bot>[] = [
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
