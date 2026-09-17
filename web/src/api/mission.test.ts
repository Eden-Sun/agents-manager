import test from 'node:test'
import assert from 'node:assert/strict'
import { toMissionDetail, toMissionEvent } from './normalize.ts'
import { isMissionGone, isMissionsUnsupported } from './index.ts'
import { ApiError } from './types.ts'

test('real daemon question/answer kinds and reply_to survive normalization', () => {
  const question = toMissionEvent({ id: 'q', kind: 'question', text: 'why?', mission_id: 'm' })
  const answer = toMissionEvent({ id: 'a', kind: 'answer', reply_to: 'q', relay_from: 'agm', text: 'because', mission_id: 'm' })
  assert.equal(question?.kind, 'question')
  assert.equal(answer?.kind, 'answer')
  assert.equal(answer?.reply_to, 'q')
  assert.equal(answer?.relay_from, 'agm')
})

/**
 * The daemon already returns mission events in write order (`ORDER BY created_at, rowid`). Re-sorting
 * them here on `(created_at, id)` used to undo that: `created_at` is millisecond-resolution and the
 * ids are ULIDs, whose random section is not monotonic inside one millisecond.
 */
test('same-millisecond events keep the order the daemon sent them in', () => {
  const at = '2026-09-17T12:00:00.000Z'
  // The ids run backwards on purpose: sorting on them would flip this pair.
  const detail = toMissionDetail({
    id: 'm', project_id: 'p',
    events: [
      { id: '01ZZZZZZZZZZZZZZZZZZZZZZZZ', kind: 'round', text: '', mission_id: 'm', created_at: at },
      { id: '01AAAAAAAAAAAAAAAAAAAAAAAA', kind: 'paused', text: '', mission_id: 'm', created_at: at },
    ],
  })
  assert.deepEqual(detail?.events.map((e) => e.kind), ['round', 'paused'])
})

test('parent and child references are retained, malformed collections cannot hide the card', () => {
  const detail = toMissionDetail({
    id: 'm', project_id: 'p', parent_mission_id: 'parent',
    parent: { id: 'parent', text: 'original', status: 'done' },
    revisions: [{ id: 'child', text: 'change', status: 'paused' }], events: {}, assignments: 'bad',
  })
  assert.equal(detail?.parent?.id, 'parent')
  assert.equal(detail?.revisions[0]?.status, 'paused')
  assert.deepEqual(detail?.events, [])
  assert.deepEqual(detail?.assignments, [])
})

/**
 * 404 有兩種，形狀不同：SPA fallback（舊 daemon 沒這組路由，body 是 index.html）與 daemon 的
 * `{"error":"not_found"}`（路由在、只是這一筆不在）。混為一談會讓一張過期的任務卡關掉整個功能。
 */
test('找不到的那一筆不算「這台 daemon 沒有群組任務」', () => {
  const gone = new ApiError(404, { error: 'not_found', what: 'mission' }, 'not found')
  assert.equal(isMissionGone(gone), true)
  assert.equal(isMissionsUnsupported(gone), false)
})

test('SPA fallback 的 404、405、501 才是沒有這組路由', () => {
  const html = new ApiError(404, { reason: '<!doctype html>…' }, 'not found')
  assert.equal(isMissionGone(html), false)
  for (const e of [html, new ApiError(405, { reason: 'Method Not Allowed' }, 'x'), new ApiError(501, {}, 'x')]) {
    assert.equal(isMissionsUnsupported(e), true)
  }
  assert.equal(isMissionsUnsupported(new ApiError(500, { error: 'upstream' }, 'x')), false)
  assert.equal(isMissionsUnsupported(new Error('network')), false)
})
