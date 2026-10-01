import test from 'node:test'
import assert from 'node:assert/strict'
import { grokUpdatePlan, GROK_UPDATE_COMMAND } from './grokUpdate.ts'
import type { UpstreamItem } from '../store/upstreamUpdate.ts'

const item = (over: Partial<UpstreamItem> = {}): UpstreamItem => ({
  kind: 'grok',
  latest: '1.0.47',
  target: '1.0.47',
  hasUpdate: true,
  hosts: [
    { host: 'local', installedVersion: '1.0.46', error: null, behind: true },
    { host: 'm4p', installedVersion: '1.0.47', error: null, behind: false },
  ],
  notify: null,
  text: 'grok 上游有新版 1.0.47',
  ...over,
})

test('grok 有新版：列出落後的主機與官方指令，沒落後的不列（#761）', () => {
  const p = grokUpdatePlan(item())
  assert.ok(p)
  assert.equal(p.target, '1.0.47')
  assert.deepEqual(p.hosts, [{ host: 'local', from: '1.0.46' }])
  assert.equal(p.command, GROK_UPDATE_COMMAND)
  assert.equal(GROK_UPDATE_COMMAND, 'grok update')
})

test('沒有更新、不是 grok、抓不到上游、沒有落後主機：不畫', () => {
  assert.equal(grokUpdatePlan(undefined), null)
  assert.equal(grokUpdatePlan(item({ hasUpdate: false })), null)
  assert.equal(grokUpdatePlan(item({ kind: 'codex' })), null)
  assert.equal(grokUpdatePlan(item({ hosts: [{ host: 'local', installedVersion: '1.0.47', error: null, behind: false }] })), null)
  assert.equal(grokUpdatePlan(item({ target: null })), null)
})

test('讀不到版本的主機也列（from 是 null），不能因此整顆提示消失', () => {
  const p = grokUpdatePlan(item({ hosts: [{ host: 'm4p', installedVersion: null, error: 'timeout', behind: true }] }))
  assert.deepEqual(p?.hosts, [{ host: 'm4p', from: null }])
})
