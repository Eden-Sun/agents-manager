import test from 'node:test'
import assert from 'node:assert/strict'
import type { Project } from '../api/types'
import { composerState, useStore } from '../store/store.ts'
import { handedOffTo } from './handoff.ts'

const project = (handed_off_to?: string | null) => ({ ...({} as Project), id: 'p1', host: 'local', handed_off_to })

test('只有非空字串才算移交出去', () => {
  assert.equal(handedOffTo([project('agm-host')], 'p1'), 'agm-host')
  for (const v of [null, undefined, '', '  ']) assert.equal(handedOffTo([project(v)], 'p1'), null)
  assert.equal(handedOffTo([project('agm-host')], 'p2'), null, '別的專案')
})

/** #708：移交出去的 bot 輸入框鎖住並說明由誰管，不提供「送出＝啟動」。 */
test('a handed-off bot locks the composer and says who manages it', () => {
  const base = useStore.getState()
  const state = {
    ...base,
    connected: true,
    bots: [{ ...({} as (typeof base.bots)[number]), id: 'b1', project_id: 'p1', herdr_session: null }],
    projects: [project('agm-host')],
    runs: {},
  }
  const cs = composerState(state, 'b1')
  assert.equal(cs.disabled, true)
  assert.equal(cs.autoStart, undefined)
  assert.match(cs.reason, /由 agm-host 管理/)
})
