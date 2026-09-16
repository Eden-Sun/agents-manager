import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Identity, Project } from '../api/types.ts'
import { findIdentity, identityRowKey, identityUseCount, shadowedByConfig } from './identityRows.ts'

const cc1 = (host: string | null, dir: string): Identity => ({ name: 'cc1', host, kind: 'claude', env: { CLAUDE_CONFIG_DIR: dir }, args: [] })
const both = [cc1(null, '/Users/me/.claude-cc1'), cc1('m4p', '/Users/m4p/.claude-ccompany')]

test('同名身分在兩台：列表 key 不重複，各自找到自己那一筆', () => {
  assert.deepEqual(both.map(identityRowKey), ['local:cc1', 'm4p:cc1'])
  assert.equal(findIdentity(both, 'local', 'cc1')?.env.CLAUDE_CONFIG_DIR, '/Users/me/.claude-cc1')
  assert.equal(findIdentity(both, 'm4p', 'cc1')?.env.CLAUDE_CONFIG_DIR, '/Users/m4p/.claude-ccompany', '不能又找到本機那筆')
  assert.equal(findIdentity(both, 'other', 'cc1'), undefined)
})

test('用量只算同一台的 bot：別台綁著同名身分不擋這一筆的刪除', () => {
  const projects = [{ id: 'pl', host: 'local' }, { id: 'pm', host: 'm4p' }] as Project[]
  const bots = [{ identity: 'cc1', project_id: 'pm' }] as Bot[]
  assert.equal(identityUseCount(bots, projects, 'local', 'cc1'), 0)
  assert.equal(identityUseCount(bots, projects, 'm4p', 'cc1'), 1)
})

test('本機的 config 身分不遮蔽別台 shell 認出來的同名 ccN', () => {
  const localOnly = [cc1(null, '/Users/me/.claude-cc1')]
  assert.equal(shadowedByConfig(localOnly, 'local', 'cc1'), true)
  assert.equal(shadowedByConfig(localOnly, 'm4p', 'cc1'), false)
})
