import assert from 'node:assert/strict'
import { test } from 'node:test'
import type { Bot, Run } from '../api/types'
import { projectDeleteBlockers } from './projectDeleteGuard.ts'

/** 只填測試會看的欄位；其餘用 cast 帶過。 */
export function projectDeleteGuardFixture(): { bots: Bot[]; runs: Record<string, Run | null> } {
  const bot = (id: string, project_id: string) => ({ id, project_id }) as Bot
  const run = (bot_id: string, state: Run['state']) => ({ id: `r-${bot_id}`, bot_id, state }) as Run
  return {
    bots: [bot('a', 'p1'), bot('b', 'p1'), bot('c', 'p1'), bot('d', 'p2')],
    runs: { a: run('a', 'running'), b: run('b', 'stopping'), c: run('c', 'stopped'), d: null },
  }
}

test('counts only bots of the project, active = starting/running/stopping', () => {
  const { bots, runs } = projectDeleteGuardFixture()
  assert.deepEqual(projectDeleteBlockers(bots, runs, 'p1'), { total: 3, active: 2 })
  assert.deepEqual(projectDeleteBlockers(bots, runs, 'p2'), { total: 1, active: 0 })
  assert.deepEqual(projectDeleteBlockers(bots, runs, 'nope'), { total: 0, active: 0 })
})

test('missing or null run is not active', () => {
  const { bots } = projectDeleteGuardFixture()
  assert.deepEqual(projectDeleteBlockers(bots, {}, 'p1'), { total: 3, active: 0 })
})
