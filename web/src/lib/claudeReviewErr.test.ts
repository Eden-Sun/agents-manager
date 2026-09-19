import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types.ts'
import { reviewErrText } from './claudeReviewErr.ts'

/** API.md：`POST /api/claude-update/review` 的 409 `message` 「就是給使用者看的原因」；`ApiError.message` 取的卻是 `reason` 代碼。 */
test('沒有協調者／沒有任務檔：顯示 daemon 給使用者看的 message，不是 no_target 代碼', () => {
  const noTarget = new ApiError(
    409,
    { error: 'conflict', reason: 'no_target', message: '找不到要派給誰：AGM_RELEASE_BOT、runtime.json 的 release_bot_id 或 responder_bot_id 都沒設。到 AGM 設定裡指定協調者。' },
    'x',
  )
  assert.match(reviewErrText(noTarget), /找不到要派給誰/)
  assert.doesNotMatch(reviewErrText(noTarget), /^no_target/)
  const noFile = new ApiError(409, { error: 'conflict', reason: 'no_task_file', message: '找不到 claude-release-task.md。照 scripts/ops/README.md 安裝之後再按一次。' }, 'x')
  assert.match(reviewErrText(noFile), /claude-release-task\.md/)
})

test('那台主機還讀不到 claude 版本（409 no_version，沒有 message）：講人話', () => {
  const e = new ApiError(409, { error: 'conflict', reason: 'no_version', host: 'local' }, 'x')
  assert.match(reviewErrText(e), /版本/)
  assert.doesNotMatch(reviewErrText(e), /no_version/)
})

test('舊 daemon（404／405）與一般錯誤照舊', () => {
  assert.match(reviewErrText(new ApiError(405, {}, 'x')), /二進位比前端舊/)
  assert.equal(reviewErrText(new Error('boom')), 'boom')
  assert.equal(reviewErrText(new ApiError(502, { error: 'upstream', message: '派工失敗：db' }, 'x')), '派工失敗：db')
})
