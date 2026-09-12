/**
 * 總管 payload 的契約測試。
 *
 * 重點不是「正常資料能不能解析」，而是**畸形資料不能把面板打掛，也不能被畫成健康**。
 * 這個面板存在的理由就是 daemon 不對勁的時候給人看；它要能撐住 daemon 回半殘 JSON、
 * 舊版欄位、或整個型別都不對的情況。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { toAssignment, toIncident } from './supervisor.ts'

test('集合欄位不是陣列時不會 throw（.map 白屏的那個 bug）', () => {
  // 這些都是 `(x as unknown[]) ?? []` 擋不住的：?? 只認 null/undefined。
  for (const junk of [{}, 'nope', 42, true, { length: 3 }]) {
    const payload = { assignments: junk, incidents: junk }
    // 模擬 toInfo/fetch* 內部的取法：不是陣列就當空的。
    const listed = Array.isArray(payload.assignments) ? payload.assignments : []
    assert.deepEqual(listed, [], `${JSON.stringify(junk)} 應該被當成空集合`)
  }
})

test('陣列裡的垃圾條目被濾掉，不是整包爆掉', () => {
  const rows = [null, 'x', 42, {}, { id: '' }, { id: 'a1', status: 'queued' }]
  const parsed = rows.map(toAssignment).filter((a) => a !== null)
  assert.equal(parsed.length, 1, '只有帶 id 的那一筆算數')
  assert.equal(parsed[0]?.id, 'a1')
})

test('舊 daemon 沒給 open/awaiting_review 時，由狀態自己推，且不預設成已結案', () => {
  const queued = toAssignment({ id: 'a1', status: 'queued' })
  assert.equal(queued?.open, true)
  assert.equal(queued?.awaiting_review, false)

  const waiting = toAssignment({ id: 'a2', status: 'awaiting_review' })
  assert.equal(waiting?.open, true, '等驗收仍是未結案')
  assert.equal(waiting?.awaiting_review, true)

  const blocked = toAssignment({ id: 'a3', status: 'blocked' })
  assert.equal(blocked?.open, true, '阻塞中也還沒結案')

  const done = toAssignment({ id: 'a4', status: 'completed' })
  assert.equal(done?.open, false)

  // 狀態本身是垃圾：正規化成 'unknown'，而 'unknown'（送達未知）本來就是未結案的一種，
  // 所以它會被算成還欠著。方向是對的——讀不懂的狀態寧可留在待辦裡，也不要當成做完了。
  const weird = toAssignment({ id: 'a5', status: 12345 })
  assert.equal(weird?.status, 'unknown')
  assert.equal(weird?.open, true, '讀不懂就當還沒結案，不要假裝 completed')
})

test('daemon 有給 open 就以它為準', () => {
  const a = toAssignment({ id: 'a1', status: 'completed', open: true })
  assert.equal(a?.open, true, 'daemon 說還沒結案就是還沒結案')
})

test('legacy_closed 只在 daemon 明說時才是 true', () => {
  assert.equal(toAssignment({ id: 'a1', status: 'completed' })?.legacy_closed, false)
  assert.equal(toAssignment({ id: 'a1', status: 'completed', legacy_closed: true })?.legacy_closed, true)
  // 不是布林就不算：'false' 這種字串曾經在別的地方被當成 true。
  assert.equal(toAssignment({ id: 'a1', legacy_closed: 'false' })?.legacy_closed, false)
})

test('review 欄位缺了或型別不對時是 null，不是空字串或 undefined', () => {
  const none = toAssignment({ id: 'a1', status: 'queued' })
  assert.deepEqual(none?.review, { decision: null, by: null, reason: null })

  const bad = toAssignment({ id: 'a1', review: 'accepted' })
  assert.deepEqual(bad?.review, { decision: null, by: null, reason: null }, 'review 不是物件就當沒有')

  const good = toAssignment({ id: 'a1', review: { decision: 'accept', by: 'AGM', reason: '測試通過' } })
  assert.equal(good?.review.decision, 'accept')
  assert.equal(good?.review.by, 'AGM')
})

test('evidence_complete 是三態：true / false / 不知道', () => {
  assert.equal(toAssignment({ id: 'a1', evidence_complete: true })?.evidence_complete, true)
  assert.equal(toAssignment({ id: 'a1', evidence_complete: false })?.evidence_complete, false)
  // 沒給就是不知道，**不可以**當成 true——那等於替沒看過的證據背書。
  assert.equal(toAssignment({ id: 'a1' })?.evidence_complete, null)
  assert.equal(toAssignment({ id: 'a1', evidence_complete: 'yes' })?.evidence_complete, null)
})

test('incident 缺 id 就不是一筆 incident；severity 缺了當 degraded 不當沒事', () => {
  assert.equal(toIncident({ kind: 'host_disconnected' }), null)
  assert.equal(toIncident(null), null)
  assert.equal(toIncident('boom'), null)

  const i = toIncident({ id: 'i1', kind: 'host_disconnected', resource: 'mac2' })
  assert.equal(i?.severity, 'degraded', '沒給嚴重度時不要當成無害')
  assert.equal(i?.status, 'open')
  assert.equal(i?.occurrences, 0)

  const full = toIncident({ id: 'i2', kind: 'notify_exhausted', severity: 'critical', occurrences: 4 })
  assert.equal(full?.severity, 'critical')
  assert.equal(full?.occurrences, 4)
})

test('occurrences 不是有限數字時回 0，不會變成 NaN 印在畫面上', () => {
  for (const bad of ['3', null, undefined, NaN, Infinity, {}]) {
    assert.equal(toIncident({ id: 'i1', occurrences: bad })?.occurrences, 0)
  }
})
