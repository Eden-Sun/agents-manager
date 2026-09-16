/** 身分是每台一份（SPEC §16.2）：沒寫 host 的只適用本機，不能遮蔽遠端同名的 `ccN`。 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { identitiesOfHost } from './store'
import type { Identity, IdentityStatusMap } from '../api/types'

const configured = (host: string | null, dir: string): Identity => ({
  name: 'cc1',
  host,
  kind: 'claude',
  env: { CLAUDE_CONFIG_DIR: dir },
  args: [],
})

const shell = (dir: string): IdentityStatusMap => ({
  cc1: {
    name: 'cc1',
    kind: 'claude',
    logged_in: true,
    reason: null,
    account: null,
    plan: null,
    source: 'shell',
    config_dir: dir,
  },
})

test('現行形狀（沒寫 host）在本機一個字都沒變', () => {
  const list = identitiesOfHost([configured(null, '/home/me/.claude-cc1')], {}, 'local')
  assert.equal(list.length, 1)
  assert.equal(list[0].env.CLAUDE_CONFIG_DIR, '/home/me/.claude-cc1')
  // 預設參數＝本機，舊呼叫端（沒帶 host）行為不變
  assert.deepEqual(identitiesOfHost([configured(null, '/home/me/.claude-cc1')], {}), list)
})

test('沒寫 host 的不再遮蔽遠端同名的 ccN', () => {
  const list = identitiesOfHost([configured(null, '/home/me/.claude-cc1')], shell('/home/m4p/.claude-ccompany'), 'm4p')
  assert.equal(list.length, 1)
  assert.equal(list[0].env.CLAUDE_CONFIG_DIR, '/home/m4p/.claude-ccompany', '那台的 cc1 才算數')
})

test('明寫 host 的只在那一台生效，而且蓋過那台的 shell 同名', () => {
  const rows = [configured('m4p', '/home/m4p/.claude-pinned')]
  const remote = identitiesOfHost(rows, shell('/home/m4p/.claude-ccompany'), 'm4p')
  assert.equal(remote.length, 1)
  assert.equal(remote[0].env.CLAUDE_CONFIG_DIR, '/home/m4p/.claude-pinned')
  assert.deepEqual(identitiesOfHost(rows, {}, 'local'), [], '別台的不會出現在本機')
})
