import test from 'node:test'
import assert from 'node:assert/strict'
import type { Message, Turn } from '../api/types'
import { noteQueuedTurn, startingSend, startingSendLabel } from './startingSend.ts'

const turn = (p: Partial<Turn>) =>
  ({ id: 't1', status: 'queued', delivery: 'pending', awaitsStart: true, startError: null, ...p }) as Turn
const msg = (p: Partial<Message>) =>
  ({ id: 'm1', turn_id: 't1', role: 'user', content: '幫我跑測試', attachments: [], ...p }) as Message

/** issue #122：重整之後 store 只有 daemon 給的 turn 與訊息——從那裡就認得出「有一則在等 bot 起來」。 */
test('a queued turn the daemon took for a stopped bot is the one waiting for the start', () => {
  const s = startingSend({ t1: turn({}) }, [msg({}), msg({ id: 'm2', role: 'system', content: '別的' })])
  assert.deepEqual(s, { turnId: 't1', text: '幫我跑測試', attachments: 0, startError: null })
  assert.equal(startingSendLabel(s!), '啟動中，起來後自動送出：')
  const failed = startingSend({ t1: turn({ startError: '找不到 claude' }) }, [msg({})])
  assert.equal(startingSendLabel(failed!), '沒能啟動（找不到 claude），還沒送出：')
  assert.equal(startingSendLabel(failed!, true), '啟動中，起來後自動送出：', '按了重新啟動、run 起來了：舊原因不算數')
})

test('AGM queued dispatches and already-sent turns are not a starting send', () => {
  assert.equal(startingSend({ t1: turn({ awaitsStart: false }) }, [msg({})]), null, 'AGM 派工排的')
  assert.equal(startingSend({ t1: turn({ status: 'in_flight' }) }, [msg({})]), null, '已經送出去了')
  assert.equal(startingSend(undefined, undefined), null)
})

test('the HTTP answer seeds the queued turn unless its frame already landed', () => {
  const s = noteQueuedTurn({ turns: {} }, 'b1', 't1', 'crid-1', true)
  assert.ok('turns' in s)
  const t = (s as { turns: Record<string, Record<string, Turn>> }).turns.b1.t1
  assert.equal(t.status, 'queued')
  assert.equal(t.awaitsStart, true)
  const later = { turns: { b1: { t1: turn({ startError: 'x' }) } } }
  assert.deepEqual(noteQueuedTurn(later, 'b1', 't1', 'crid-1', true), {}, 'frame 先到的比較新，不蓋')
})

test('the daemon turn JSON carries awaits_start and start_error through to the store', async () => {
  const { toTurn } = await import('../api/normalize.ts')
  const t = toTurn({ id: 't1', status: 'queued', delivery: 'pending', awaits_start: 1, start_error: '找不到 claude' }, 'b1')!
  assert.equal(t.awaitsStart, true)
  assert.equal(t.startError, '找不到 claude')
  const old = toTurn({ id: 't2', status: 'queued', delivery: 'pending' }, 'b1')!
  assert.equal(old.awaitsStart, false, '舊 daemon 沒有這一欄：當作 AGM 的排隊')
  assert.equal(old.startError, null)
})
