import test from 'node:test'
import assert from 'node:assert/strict'
import type { Run } from '../api/types'
import { backgroundDetail, backgroundJobs, backgroundLabel, backgroundShortLabel } from './backgroundJobs.ts'

const run = (over: Partial<Run>) => ({ ...({} as Run), state: 'running', agent_status: 'idle', background_jobs: 1, ...over }) as Run

test('只有「在跑、閒著、還有背景工作」才算背景執行中', () => {
  assert.equal(backgroundJobs(run({})), 1)
  assert.equal(backgroundJobs(run({ background_jobs: 3 })), 3)
  assert.equal(backgroundJobs(run({ agent_status: 'working' })), 0, '回合中燈號已經說了')
  assert.equal(backgroundJobs(run({ state: 'stopping' })), 0)
  assert.equal(backgroundJobs(run({ background_jobs: 0 })), 0)
  assert.equal(backgroundJobs(run({ background_jobs: undefined })), 0, '舊 daemon 沒帶這欄')
  assert.equal(backgroundJobs(null), 0)
})

test('字面依 kind 說 shell 或背景終端', () => {
  assert.equal(backgroundLabel(2), '背景執行中（2）')
  assert.equal(backgroundShortLabel(2), '背景 2')
  assert.match(backgroundDetail('claude', 1), /背景還有 1 個 shell 在跑/)
  assert.match(backgroundDetail('codex', 2), /背景還有 2 個終端在跑/)
})
