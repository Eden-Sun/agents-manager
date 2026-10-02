import test from 'node:test'
import assert from 'node:assert/strict'
import type { Run } from '../api/types'
import { backgroundDetail, backgroundJobs, backgroundLabel, backgroundShortLabel, backgroundTaskLines, cronLabel } from './backgroundJobs.ts'

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

test('claude 的 Stop hook 報的明細：最多三行，用描述（沒有就用命令），其餘折成「另有 N 個」', () => {
  const t = (description: string, command?: string, type = 'shell') => ({ id: description, type, status: 'running', description, ...(command ? { command } : {}) })
  assert.deepEqual(backgroundTaskLines(run({})), [], '畫面判斷的數字沒有明細')
  assert.deepEqual(backgroundTaskLines(run({ background_tasks: [] })), [])
  assert.deepEqual(backgroundTaskLines(run({ background_tasks: [t('等遠端 build'), t('', 'sleep 600'), t('review', undefined, 'subagent')] })), [
    'shell：等遠端 build',
    'shell：sleep 600',
    'subagent：review',
  ])
  const many = run({ background_tasks: [t('a'), t('b'), t('c'), t('d'), t('e')] })
  assert.deepEqual(backgroundTaskLines(many), ['shell：a', 'shell：b', 'shell：c', '另有 2 個'])
})

test('session_crons 只是資訊：一行字，沒有就不畫', () => {
  assert.equal(cronLabel(run({})), null)
  assert.equal(cronLabel(run({ session_crons: [] })), null)
  assert.equal(cronLabel(run({ session_crons: [{ id: 'c', schedule: '0 9 * * *', recurring: true, prompt: 'x' }] })), '另有 1 個排程會叫醒它')
})
