import test from 'node:test'
import assert from 'node:assert/strict'
import { composerState, useStore } from './store.ts'
import { UNKNOWN_STUCK_REASON, UNKNOWN_WORKING_REASON, unknownLock } from './deliveryUnknown.ts'

/** issue #1479：delivery=unknown 但 bot 正在跑只是「還沒確認」；閒著且輸入框仍鎖著才是真的卡住。 */
test('unknownLock: working is neutral, anything else keeps the abandon advice', () => {
  assert.deepEqual(unknownLock('working'), { reason: UNKNOWN_WORKING_REASON, working: true })
  for (const s of ['idle', 'blocked', 'unknown', '', null, undefined]) {
    assert.deepEqual(unknownLock(s), { reason: UNKNOWN_STUCK_REASON, working: false }, String(s))
  }
  assert.doesNotMatch(UNKNOWN_WORKING_REASON, /放棄/, '中性提示不叫人放棄回合')
  assert.match(UNKNOWN_STUCK_REASON, /放棄該回合/)
})

function stateWith(agentStatus: string, turn: Record<string, unknown>) {
  const base = useStore.getState()
  return {
    ...base,
    connected: true,
    bots: [{ ...({} as (typeof base.bots)[number]), id: 'b1', project_id: 'p1', herdr_session: null }],
    projects: [{ ...({} as (typeof base.projects)[number]), id: 'p1', host: 'local' }],
    runs: { b1: { id: 'r1', bot_id: 'b1', state: 'running', agent_status: agentStatus } as unknown as (typeof base.runs)[string] },
    turns: { b1: { t1: { id: 't1', run_id: 'r1', ...turn } as unknown as (typeof base.turns)[string][string] } },
    messages: {},
  }
}

test('composerState: an in-flight unknown turn on a working bot is "not confirmed yet", not "abandon it"', () => {
  const cs = composerState(stateWith('working', { status: 'in_flight', delivery: 'unknown' }), 'b1')
  assert.equal(cs.disabled, true, '送達未確認前輸入框仍鎖著（daemon 還會 409）')
  assert.equal(cs.reason, UNKNOWN_WORKING_REASON)
  assert.equal(cs.unknownWorking, true)
  assert.equal(cs.unknownTurnId, 't1', '逃生口（解除卡對話）仍認得這一回合')
})

test('composerState: an in-flight unknown turn on an idle bot keeps the abandon advice', () => {
  const cs = composerState(stateWith('idle', { status: 'in_flight', delivery: 'unknown' }), 'b1')
  assert.equal(cs.disabled, true)
  assert.equal(cs.reason, UNKNOWN_STUCK_REASON)
  assert.equal(cs.unknownWorking, false)
  assert.equal(cs.unknownTurnId, 't1')
})

test('composerState: once the daemon promotes the turn to ok the lock is gone', () => {
  const cs = composerState(stateWith('working', { status: 'in_flight', delivery: 'ok' }), 'b1')
  assert.equal(cs.unknownTurnId, null)
  assert.equal(cs.disabled, false)
  assert.equal(cs.queued, true)
})
