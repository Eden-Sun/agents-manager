import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types'
import { BASE_SNAPSHOT_REASON_TEXT, RETIRE_REASON_TEXT, humanMessage, isLegacyDaemonError, plainErrMsg, reasonTextBySubstring } from './errText'

const apiErr = (status: number, body: Record<string, unknown>) => new ApiError(status, body, 'fb')

test('plainErrMsg', () => {
  assert.equal(plainErrMsg(new Error('x')), 'x')
  assert.equal(plainErrMsg('y'), 'y')
})
test('humanMessage trims and rejects blanks', () => {
  assert.equal(humanMessage(apiErr(409, { message: '  hi ' })), 'hi')
  assert.equal(humanMessage(apiErr(409, { message: '  ' })), null)
  assert.equal(humanMessage(new Error('x')), null)
})
test('isLegacyDaemonError', () => {
  assert.equal(isLegacyDaemonError(apiErr(405, {})), true)
  assert.equal(isLegacyDaemonError(apiErr(404, {})), true)
  assert.equal(isLegacyDaemonError(apiErr(404, { what: 'bot' })), false)
  assert.equal(isLegacyDaemonError(apiErr(409, {})), false)
})
test('reason tables', () => {
  assert.equal(reasonTextBySubstring('x api_retire_busy y', RETIRE_REASON_TEXT), 'bot 仍有進行中的回合或關閉尚未完成，請稍後再試')
  assert.equal(reasonTextBySubstring('base_snapshot_not_codex', BASE_SNAPSHOT_REASON_TEXT), '基準快照只支援 codex Bot')
  assert.equal(reasonTextBySubstring('other', RETIRE_REASON_TEXT), null)
})
