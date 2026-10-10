/**
 * 「側欄看得到哪些 bot」只有一份定義：額度停用收起來的那一批，鍵盤導覽與分頁標題的 `(N)`
 * 也要當它們不在，否則點得到一顆側欄找不到的 bot、數字也對不起來。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { quotaDisableKey, quotaHiddenBotIds } from './quotaHide.ts'
import { adjacentBotId, orderedBotIds } from './store.ts'
import { titleUnread } from './unread.ts'
import type { StoreState } from './store.ts'
import type { Bot, Project } from '../api/types.ts'

const bot = (id: string, identity: string | null, extra: Partial<Bot> = {}) =>
  ({ id, name: id, project_id: 'p1', kind: 'claude', identity, ...extra }) as Bot
const project = { id: 'p1', label: 'p', path: '/p', host: 'local' } as Project

const state = (selectedBotId: string | null): StoreState =>
  ({
    projects: [project],
    projectOrder: [],
    bots: [bot('b1', 'cc1'), bot('b2', 'cc1'), bot('b3', null)],
    botOrder: {},
    selectedBotId,
  }) as unknown as StoreState

const disabledCc1 = { [quotaDisableKey('local', 'claude', 'cc1')]: null }

test('停用的身分整批收起來', () => {
  assert.deepEqual(quotaHiddenBotIds(state(null), disabledCc1), ['b1', 'b2'])
  assert.deepEqual(quotaHiddenBotIds(state(null), {}), [])
})

test('正在看的那一顆不收：主面板還開著它的對話，側欄卻找不到那一列', () => {
  assert.deepEqual(quotaHiddenBotIds(state('b1'), disabledCc1), ['b2'])
})

test('⌥↑／⌥↓ 跳過收起來的 bot，不會走進畫面上沒有的那一列', () => {
  const s = { ...state(null), hiddenBotIds: ['b2'] }
  assert.deepEqual(orderedBotIds(s), ['b1', 'b3'])
  assert.equal(adjacentBotId(s, 'b1', 1), 'b3')
  assert.equal(adjacentBotId(s, 'b3', 1), 'b1')
})

test('分頁標題的 (N) 不算收起來的未讀', () => {
  const book = { ...state(null), botUnread: { b1: 2, b2: 3, b3: 1 }, supervisorProjectId: null }
  assert.equal(titleUnread({ ...book, hiddenBotIds: [] }), 6)
  assert.equal(titleUnread({ ...book, hiddenBotIds: ['b1', 'b2'] }), 1)
})

// #1057：⌥↑／⌥↓ 的順序要跟側欄畫的一樣：子 agent 緊跟在父列後面。
const tree = (botOrder: Record<string, string[]> = {}, hidden: string[] = []) =>
  ({
    ...state(null),
    bots: [
      bot('P1', null),
      bot('P2', null),
      bot('c1', null, { parent_bot_id: 'P1' }),
      bot('c2', null, { parent_bot_id: 'P2' }),
      bot('c1b', null, { parent_bot_id: 'P1' }),
    ],
    botOrder,
    hiddenBotIds: hidden,
  }) as unknown as StoreState

test('鍵盤順序跟側欄畫的一樣：子 agent 緊跟在父列後面', () => {
  const s = tree()
  assert.deepEqual(orderedBotIds(s), ['P1', 'c1', 'c1b', 'P2', 'c2'])
  assert.equal(adjacentBotId(s, 'P1', 1), 'c1')
  assert.equal(adjacentBotId(s, 'P2', -1), 'c1b')
})

test('拖過順序的專案也一樣：父列照拖的順序，子 agent 仍跟在父列後面', () => {
  assert.deepEqual(orderedBotIds(tree({ p1: ['P2', 'P1'] })), ['P2', 'c2', 'P1', 'c1', 'c1b'])
})

test('父列被收起或不在清單裡的子 agent 仍走得到', () => {
  const hiddenParent = orderedBotIds(tree({}, ['P1']))
  assert.ok(!hiddenParent.includes('P1'))
  assert.ok(hiddenParent.includes('c1') && hiddenParent.includes('c1b'))

  const orphan = { ...state(null), bots: [bot('P1', null), bot('gone-child', null, { parent_bot_id: 'gone' })] } as unknown as StoreState
  assert.ok(orderedBotIds(orphan).includes('gone-child'))
})
