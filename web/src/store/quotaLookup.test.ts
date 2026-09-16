import test from 'node:test'
import assert from 'node:assert/strict'
import { bareQuotaOwner, quotaBaseKey, quotaForIdentity } from './quotaLookup.ts'
import { botQuotaLevel, botQuotaWarning } from './store.ts'
import type { Identity, KindQuota, QuotaMap } from '../api/types.ts'

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
