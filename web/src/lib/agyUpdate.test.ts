import test from 'node:test'
import assert from 'node:assert/strict'
import { agyChipLabel, agyMenuItem, agyUpdatePlan } from './agyUpdate.ts'
import type { UpstreamItem } from '../store/upstreamUpdate.ts'

const item = (over: Partial<UpstreamItem> = {}): UpstreamItem => ({
  kind: 'agy',
  latest: '1.4.6',
  target: '1.4.6',
  hasUpdate: true,
  hosts: [{ host: 'local', installedVersion: '1.4.5', error: null, behind: true }],
  notify: 'update',
  text: null,
  ...over,
})

test('agy 有新版且有落後主機才出 plan；只列落後的主機（#1921）', () => {
  const plan = agyUpdatePlan(
    item({
      hosts: [
        { host: 'local', installedVersion: '1.4.5', error: null, behind: true },
        { host: 'm4p', installedVersion: '1.4.6', error: null, behind: false },
        { host: 'gpu', installedVersion: null, error: 'ssh 中斷', behind: false },
      ],
    }),
  )
  assert.deepEqual(plan, { target: '1.4.6', hosts: [{ host: 'local', from: '1.4.5' }] })
})

test('沒有新版、沒有落後主機、或不是 agy 的項目都不出 chip', () => {
  assert.equal(agyUpdatePlan(null), null)
  assert.equal(agyUpdatePlan(item({ hasUpdate: false })), null)
  assert.equal(agyUpdatePlan(item({ target: null })), null)
  assert.equal(agyUpdatePlan(item({ hosts: [{ host: 'local', installedVersion: '1.4.6', error: null, behind: false }] })), null)
  assert.equal(agyUpdatePlan(item({ kind: 'grok' })), null)
})

test('tooltip 寫新版、每台版本與安裝方式；讀不到版本寫明', () => {
  const t = agyChipLabel({ target: '1.4.6', hosts: [{ host: 'local', from: '1.4.5' }, { host: 'm4p', from: null }] })
  assert.match(t, /agy 有新版 1\.4\.6/)
  assert.match(t, /local：1\.4\.5；m4p：讀不到版本/)
  assert.match(t, /sha512/)
  assert.match(t, /重啟後才換版/)
})

test('手機合成選單的 agy 那一項', () => {
  assert.equal(agyMenuItem({ target: '1.4.6', hosts: [{ host: 'local', from: '1.4.5' }, { host: 'm4p', from: '1.4.5' }] }), 'agy 1.4.6：local、m4p 可更新（點開確認）')
})
