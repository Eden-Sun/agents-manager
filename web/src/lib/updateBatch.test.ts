import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Run } from '../api/types.ts'
import { updateBatchCounts } from './updateBatch.ts'

const NOTICE = 'Update installed · Restart to update'

const bot = (id: string, over: Partial<Bot> = {}): Bot =>
  ({ id, name: id, kind: 'claude', managed_by: 'user', ...over }) as Bot

const run = (id: string, over: Partial<Run> = {}): Run =>
  ({ id: `r-${id}`, bot_id: id, state: 'running', agent_status: 'idle', update_notice: NOTICE, ...over }) as Run

const none = () => false

test('閒置且帶著更新的才算「可以重啟」', () => {
  const bots = [bot('a'), bot('b')]
  const runs = { a: run('a'), b: run('b') }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['a', 'b'],
  )
  assert.deepEqual(c.busy, [])
})

test('working / blocked 進「在忙」而不是「可以重啟」', () => {
  const bots = [bot('idle'), bot('busy'), bot('asking')]
  const runs = {
    idle: run('idle'),
    busy: run('busy', { agent_status: 'working' }),
    asking: run('asking', { agent_status: 'blocked' }),
  }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['idle'],
  )
  assert.deepEqual(
    c.busy.map((x) => [x.name, x.why]),
    [
      ['busy', '正在跑'],
      ['asking', '卡在提問，等人回答'],
    ],
  )
})

test('回合還在飛的即使 idle 也不動；starting / stopping 同理', () => {
  const bots = [bot('mid'), bot('booting')]
  const runs = { mid: run('mid'), booting: run('booting', { state: 'starting' }) }
  const c = updateBatchCounts(bots, runs, (id) => id === 'mid')
  assert.deepEqual(c.ready, [])
  assert.deepEqual(
    c.busy.map((x) => x.why),
    ['還有一回合沒收掉', '還在啟動或關閉中'],
  )
})

test('沒有更新在等、grok、沒在跑的都不進帳', () => {
  const bots = [bot('gk', { kind: 'grok' }), bot('clean'), bot('stopped')]
  const runs = { gk: run('gk'), clean: run('clean', { update_notice: null }) }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(c.ready, [])
  assert.deepEqual(c.busy, [])
})

test('2026-09-22：codex 磁碟已裝好新版跟 claude 一樣可以重啟；還沒裝的算「在忙」不算消失', () => {
  const bots = [bot('cx-ok', { kind: 'codex' }), bot('cx-wait', { kind: 'codex' })]
  const runs = {
    'cx-ok': run('cx-ok', { update_notice: 'codex 有新版 0.154.0（這個 run 跑的是 0.154.0），已安裝，重啟套用' }),
    'cx-wait': run('cx-wait', { update_notice: 'codex 有新版 0.154.0 → 0.155.1，需安裝後重啟' }),
  }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['cx-ok'],
  )
  assert.deepEqual(
    c.busy.map((x) => [x.name, x.why]),
    [['cx-wait', '新版還沒裝，要先手動安裝才能套用']],
  )
})

test('子 agent 也歸這顆按鈕管（587b07f：在自己的 pane 裡 exit + resume）', () => {
  const bots = [bot('kid', { managed_by: 'child' }), bot('mine')]
  const runs = { kid: run('kid'), mine: run('mine') }
  const c = updateBatchCounts(bots, runs, none)
  assert.deepEqual(
    c.ready.map((x) => x.name),
    ['kid', 'mine'],
  )
  assert.deepEqual(c.busy, [])
})
