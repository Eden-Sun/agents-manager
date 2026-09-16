import test from 'node:test'
import assert from 'node:assert/strict'
import { bareQuotaOwner, quotaBaseKey, quotaForIdentity } from './quotaLookup.ts'
import { botQuotaLevel, botQuotaWarning, quotaClaimantsOf } from './store.ts'
import { identityPrefKey } from '../api/index.ts'
import type { Identity, IdentityStatusMap, KindQuota, QuotaMap } from '../api/types.ts'

const idn = (name: string, env: Record<string, string> = {}): Identity => ({ name, kind: 'claude', env, args: [] })
const win = (usedPct: number) => ({ used_pct: usedPct, low: usedPct >= 80, critical: usedPct >= 97, resets_at: null })
const q = (usedPct: number) => ({ five_hour: win(usedPct), seven_day: null, fable: null }) as unknown as KindQuota

/**
 * 回歸：`[[identities]] name = "main"`、env 留空是 SPEC §16 的合法設定，它的 statusline 數字落在
 * 裸的 `claude` 上。以前側欄只認字面上的 `cc0`，於是頂端畫紅燈、側欄一片正常。
 */
test('env 留空的身分（名字不是 cc0）也認得裸 key，頂端與側欄查到同一格', () => {
  const quota: QuotaMap = { claude: q(97) }
  const identities = [idn('main')]
  assert.equal(quotaBaseKey(quota, 'local', 'claude', 'main', identities), 'claude')
  assert.equal(botQuotaWarning(quota, 'claude', 'main', 'local', identities)?.critical, true)
  assert.equal(botQuotaLevel(quota, 'claude', 'main', 'local', null, identities)?.level, 'crit')
})

test('裸 key 只給一個身分：其他身分查不到自己那格就是沒有數字', () => {
  const quota: QuotaMap = { claude: q(97) }
  const identities = [idn('cc0'), idn('main'), idn('cc1', { CLAUDE_CONFIG_DIR: '/x' })]
  assert.equal(bareQuotaOwner(identities, 'claude'), 'cc0')
  assert.equal(quotaBaseKey(quota, 'local', 'claude', 'cc0', identities), 'claude')
  assert.equal(quotaBaseKey(quota, 'local', 'claude', 'main', identities), 'claude:main')
  assert.equal(quotaForIdentity(quota, 'local', 'claude', 'cc1', identities), null)
  assert.equal(botQuotaLevel(quota, 'claude', 'cc1', 'local', null, identities), null)
})

test('有自己那一格就用自己的，不再退回裸 key', () => {
  const quota: QuotaMap = { claude: q(10), 'claude:cc0': q(97) }
  assert.equal(quotaBaseKey(quota, 'local', 'claude', 'cc0', [idn('cc0')]), 'claude:cc0')
  assert.equal(botQuotaLevel(quota, 'claude', 'cc0', 'local', null, [idn('cc0')])?.level, 'crit')
})

test('額度按主機分（SPEC §14）：遠端只看它自己那台的數字', () => {
  const quota: QuotaMap = { claude: q(97), 'm4p/claude': q(10) }
  assert.equal(quotaForIdentity(quota, 'm4p', 'claude', 'cc0', [idn('cc0')]), quota['m4p/claude'])
  assert.equal(botQuotaLevel(quota, 'claude', 'cc0', 'm4p', null, [idn('cc0')]), null)
})

test('身分清單還沒到時保底沿用 cc0 這條字面規則', () => {
  assert.equal(bareQuotaOwner([], 'claude'), 'cc0')
  assert.equal(quotaBaseKey({ claude: q(97) }, 'local', 'claude', 'cc0', []), 'claude')
})

/**
 * 第二輪 review M1：停用 cc0 之後，頂端（先濾停用）把裸 key 給了 `main` 畫紅燈，側欄（沒濾）還是給 cc0，
 * `main` 的 bot 查 `claude:main` 查不到而不反灰。兩邊的認領清單現在都是 `quotaClaimantsOf`。
 */
test('停用 cc0 之後，頂端與側欄對 main 查到同一格', () => {
  const quota: QuotaMap = { claude: q(97) }
  const cc0Shell: IdentityStatusMap = {
    cc0: { name: 'cc0', kind: 'claude', logged_in: true, reason: null, account: null, plan: null, source: 'shell', config_dir: null },
  }
  const disabled = [identityPrefKey('local', 'claude', 'cc0')]
  const claimants = quotaClaimantsOf([idn('main')], cc0Shell, disabled, 'local')
  assert.deepEqual(claimants.map((i) => i.name), ['main'], '停用的不參與認領')
  // 頂端那一格的 key 與側欄 bot 的警告查的是同一份清單
  assert.equal(quotaBaseKey(quota, 'local', 'claude', 'main', claimants), 'claude')
  assert.equal(botQuotaWarning(quota, 'claude', 'main', 'local', claimants)?.critical, true)
})

test('兩個都不叫 cc0 的空 env 身分：誰認領裸 key 不看傳進來的順序', () => {
  assert.equal(bareQuotaOwner([idn('zeta'), idn('alpha')], 'claude'), 'alpha')
  assert.equal(bareQuotaOwner([idn('alpha'), idn('zeta')], 'claude'), 'alpha')
})

/** 跟 daemon `quota::identity_shares_default` 同一條：看那個 kind 的 home 變數，不看名字。 */
test('只帶 API key 的身分仍用預設帳號；叫 cc0 卻設了自己 config dir 的有自己那一格', () => {
  const quota: QuotaMap = { claude: q(97), 'claude:cc0': q(10) }
  const apiKeyOnly = idn('work', { ANTHROPIC_API_KEY: 'x' })
  const ownCc0 = idn('cc0', { CLAUDE_CONFIG_DIR: '/home/me/.claude-cc0' })
  assert.equal(bareQuotaOwner([apiKeyOnly, ownCc0], 'claude'), 'work', 'cc0 有自己的帳號，裸 key 不歸它')
  assert.equal(quotaBaseKey(quota, 'local', 'claude', 'work', [apiKeyOnly, ownCc0]), 'claude')
  assert.equal(quotaBaseKey(quota, 'local', 'claude', 'cc0', [apiKeyOnly, ownCc0]), 'claude:cc0')
})

test('codex bot 身上掛 claude 的 cc1：那不是 codex 的帳號代號，一律看裸 codex', () => {
  const quota: QuotaMap = { codex: q(50) }
  const cc1: Identity = { name: 'cc1', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '/x' }, args: [] }
  assert.equal(quotaBaseKey(quota, 'local', 'codex', 'cc1', [cc1]), 'codex')
})
