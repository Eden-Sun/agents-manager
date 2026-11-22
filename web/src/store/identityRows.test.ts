import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Identity, Project } from '../api/types.ts'
import { effectiveIdentity, findIdentity, identitiesForHost, identityRowKey, identityUseCount, shadowedByConfig } from './identityRows.ts'

const cc1 = (host: string | null, dir: string): Identity => ({ name: 'cc1', host, kind: 'claude', env: { CLAUDE_CONFIG_DIR: dir }, args: [] })
const both = [cc1(null, '/Users/me/.claude-cc1'), cc1('mac_builder', '/Users/me/.claude-ccompany')]

test('同名身份在兩台：列表 key 不重複，各自找到自己那一筆', () => {
  assert.deepEqual(both.map(identityRowKey), ['local:cc1', 'mac_builder:cc1'])
  assert.equal(findIdentity(both, 'local', 'cc1')?.env.CLAUDE_CONFIG_DIR, '/Users/me/.claude-cc1')
  assert.equal(findIdentity(both, 'mac_builder', 'cc1')?.env.CLAUDE_CONFIG_DIR, '/Users/me/.claude-ccompany', '不能又找到本機那筆')
  assert.equal(findIdentity(both, 'other', 'cc1'), undefined)
})

test('用量只算同一台的 bot：別台綁著同名身份不擋這一筆的刪除', () => {
  const projects = [{ id: 'pl', host: 'local' }, { id: 'pm', host: 'mac_builder' }] as Project[]
  const bots = [{ identity: 'cc1', project_id: 'pm' }] as Bot[]
  assert.equal(identityUseCount(bots, projects, 'local', 'cc1'), 0)
  assert.equal(identityUseCount(bots, projects, 'mac_builder', 'cc1'), 1)
})

test('本機的 config 身份不遮蔽別台 shell 認出來的同名 ccN', () => {
  const localOnly = [cc1(null, '/Users/me/.claude-cc1')]
  assert.equal(shadowedByConfig(localOnly, 'local', 'cc1'), true)
  assert.equal(shadowedByConfig(localOnly, 'mac_builder', 'cc1'), false)
})

test('未指定 host 的共用身份在遠端分頁也會出現，並讓位給遠端同名 shell 或明確 host 設定', () => {
  const ag1: Identity = { name: 'ag1', host: null, kind: 'agy', env: { HOME: '$HOME/.agy-ag1' }, args: [] }
  const ag2: Identity = { name: 'ag2', host: null, kind: 'agy', env: { HOME: '$HOME/.agy-ag2' }, args: [] }
  const localCc = cc1(null, '$HOME/.claude-cc1')
  const list = [ag1, ag2, localCc]
  const remoteStatuses = {
    cc1: { name: 'cc1', kind: 'claude' as const, logged_in: true, source: 'shell' as const },
    ag1: { name: 'ag1', kind: 'agy' as const, logged_in: true, source: 'config' as const },
    ag2: { name: 'ag2', kind: 'agy' as const, logged_in: false, source: 'config' as const },
  }
  assert.deepEqual(identitiesForHost(list, 'm4p', remoteStatuses).map((i) => i.name), ['ag1', 'ag2'])
  assert.equal(effectiveIdentity(list, 'm4p', 'ag2')?.name, 'ag2')
})
