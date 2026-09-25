import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types.ts'
import { deployNowNotice, deployVisible, runningText, type DeployStatus } from './deployNow.ts'

const status = (o: Partial<DeployStatus>): DeployStatus => ({
  live_sha: 'aaaa1111',
  target_sha: 'b'.repeat(40),
  target_short: 'bbbbbbbb',
  behind: 3,
  code_commits: 1,
  code_changed: true,
  commits: [],
  commits_truncated: false,
  running: null,
  working: [],
  log_path: '/x/daemon-update.log',
  kick_ready: true,
  error: null,
  ...o,
})

test('落後且有程式碼差異才出現按鈕', () => {
  assert.equal(deployVisible(null), false)
  assert.equal(deployVisible(status({})), true)
  // docs-only 的落後不算（kick 同一條規則）。
  assert.equal(deployVisible(status({ code_changed: false })), false)
  assert.equal(deployVisible(status({ behind: 0, code_changed: false })), false)
})

test('部署在跑時照樣出現（寫部署中），不因為差異算不出來就消失', () => {
  assert.equal(deployVisible(status({ code_changed: false, behind: 0, running: { kind: 'lease', resource: 'restart', owner: 'b1' } })), true)
  assert.match(runningText({ kind: 'lease', resource: 'restart', owner: 'b1' }), /b1.*restart/)
  assert.match(runningText({ kind: 'requested', sha: 'c'.repeat(40) }), /cccccccc/)
  assert.match(runningText({ kind: 'assignment', client_request_id: 'agm-daemon-update-x', status: 'delivered' }), /agm-daemon-update-x/)
})

test('送出的結果分得清楚：開始了／已經在跑／不用部署／其他錯', () => {
  const ok = deployNowNotice({ short: 'bbbbbbbb', log_path: '/x/log' }, null)
  assert.equal(ok.level, 'info')
  assert.match(ok.text, /bbbbbbbb.*\/x\/log/)
  const busy = deployNowNotice(null, new ApiError(409, { error: 'conflict', reason: 'deploy_in_progress', message: '已經有部署在跑' }, 'x'))
  assert.deepEqual([busy.level, /沒有再開一趟/.test(busy.text), /已經有部署在跑/.test(busy.text)], ['error', true, true])
  const none = deployNowNotice(null, new ApiError(409, { error: 'conflict', reason: 'nothing_to_deploy', message: '只動到文件' }, 'x'))
  assert.equal(none.level, 'info')
  const old = deployNowNotice(null, new ApiError(503, { reason: 'kick_outdated', message: '先 install 新版' }, 'x'))
  assert.match(old.text, /沒有開始：先 install 新版/)
  assert.match(deployNowNotice(null, new TypeError('Failed to fetch')).text, /Failed to fetch/)
})
