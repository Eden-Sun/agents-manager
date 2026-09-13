import test from 'node:test'
import assert from 'node:assert/strict'
import { toMissionDetail, toMissionEvent } from './normalize.ts'

test('real daemon question/answer kinds and reply_to survive normalization', () => {
  const question = toMissionEvent({ id: 'q', kind: 'question', text: 'why?', mission_id: 'm' })
  const answer = toMissionEvent({ id: 'a', kind: 'answer', reply_to: 'q', relay_from: 'agm', text: 'because', mission_id: 'm' })
  assert.equal(question?.kind, 'question')
  assert.equal(answer?.kind, 'answer')
  assert.equal(answer?.reply_to, 'q')
  assert.equal(answer?.relay_from, 'agm')
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
