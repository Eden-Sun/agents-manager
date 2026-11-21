import test from 'node:test'
import assert from 'node:assert/strict'
import { composerState, useStore } from './store.ts'

test('a stopped bot accepts a send and asks for an auto start', () => {
  const base = useStore.getState()
  const state = {
    ...base,
    connected: true,
    bots: [{ ...({} as (typeof base.bots)[number]), id: 'b1', project_id: 'p1', herdr_session: null }],
    projects: [{ ...({} as (typeof base.projects)[number]), id: 'p1', host: 'local' }],
    runs: {},
  }
  const cs = composerState(state, 'b1')
  assert.equal(cs.disabled, false)
  assert.equal(cs.queued, true)
  assert.equal(cs.autoStart, true)
})

test('a stopped bot that already holds a starting send says so instead of starting again', () => {
  const base = useStore.getState()
  const state = {
    ...base,
    connected: true,
    bots: [{ ...({} as (typeof base.bots)[number]), id: 'b1', project_id: 'p1', herdr_session: null }],
    projects: [{ ...({} as (typeof base.projects)[number]), id: 'p1', host: 'local' }],
    runs: {},
    turns: { b1: { t1: { id: 't1', status: 'queued', awaitsStart: true, startError: null } as (typeof base.turns)[string][string] } },
    messages: {},
  }
  const cs = composerState(state, 'b1')
  assert.equal(cs.disabled, false)
  assert.equal(cs.queued, true)
  assert.equal(cs.autoStart, undefined, '不會再觸發一次啟動')
  assert.equal(cs.reason, '啟動中，起來後自動送出')
})
