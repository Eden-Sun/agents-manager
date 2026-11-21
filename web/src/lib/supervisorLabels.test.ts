import test from 'node:test'
import assert from 'node:assert/strict'
import { ASSIGN_LABEL, INCIDENT_LABEL } from '../lib/supervisorLabels.ts'

test('每一種交辦狀態都有中文標籤（含 quota_blocked）', () => {
  for (const k of ['queued', 'delivered', 'unknown', 'awaiting_review', 'blocked', 'quota_blocked', 'completed', 'failed', 'cancelled', 'superseded']) {
    assert.ok(ASSIGN_LABEL[k], `缺 ${k}`)
  }
})

test('每一種 incident kind 都有標籤', () => {
  for (const k of [
    'host_disconnected', 'bot_stopped', 'assignment_stalled', 'assignment_undelivered', 'notify_exhausted', 'remote_entry',
    'approval_stalled', 'role_unavailable', 'responder_undeliverable', 'inbox_classify_failing', 'remote_shim_stale', 'remote_spool_stuck',
  ]) {
    assert.ok(INCIDENT_LABEL[k], `缺 ${k}`)
  }
})
