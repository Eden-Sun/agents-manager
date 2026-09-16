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

/** daemon dca3c4c：沒寫 host 的 config 身分**所有主機**都適用，只讓位給那台同名的（`tools::merge_identities`）。 */
test('沒寫 host 的身分遠端也拿得到，但讓位給那台同名的 shell ccN', () => {
  const main: Identity = { name: 'main', host: null, kind: 'claude', env: {}, args: [] }
  const list = identitiesOfHost([configured(null, '/home/me/.claude-cc1'), main], shell('/home/m4p/.claude-ccompany'), 'm4p')
  assert.deepEqual(list.map((i) => i.name), ['cc1', 'main'], '遠端 shell 的 cc1 先、沒寫 host 的 main 也在')
  assert.equal(list[0].env.CLAUDE_CONFIG_DIR, '/home/m4p/.claude-ccompany', '同名時那台自己的贏')
})

test('遠端還沒偵測過 alias：沒寫 host 的 ccN 先不給，偵測完才補上沒有的那幾個', () => {
  const cc2: Identity = { name: 'cc2', host: null, kind: 'claude', env: { CLAUDE_CONFIG_DIR: '/home/me/.claude-cc2' }, args: [] }
  assert.deepEqual(identitiesOfHost([cc2], {}, 'm4p'), [], '還沒偵測過，不能先把本機的 cc2 塞給 m4p')
  assert.deepEqual(identitiesOfHost([cc2], shell('/home/m4p/.claude-ccompany'), 'm4p').map((i) => i.name), ['cc1', 'cc2'])
})

test('本機的優先序：明寫 local 的蓋過沒寫 host 的，沒寫 host 的蓋過 shell 同名', () => {
  const pinned: Identity = { name: 'cc1', host: 'local', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '/pinned' }, args: [] }
  const loose = configured(null, '/loose')
  const list = identitiesOfHost([loose, pinned], shell('/shell'), 'local')
  assert.equal(list.length, 1)
  assert.equal(list[0].env.CLAUDE_CONFIG_DIR, '/pinned')
  assert.equal(identitiesOfHost([loose], shell('/shell'), 'local')[0].env.CLAUDE_CONFIG_DIR, '/loose')
})
