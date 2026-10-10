/** 額度列只把焦點標在目前 bot 實際使用的帳號格。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

const now = new Date().toISOString()
const win = { used_pct: 10, low: false, critical: false, resets_at: null, observed_at: null }
const quotaFor = (...keys: string[]) => Object.fromEntries(keys.map((key) => [key, {
  five_hour: win,
  seven_day: win,
  fable: null,
  reset_credits: null,
  limit_hit: null,
  plan: null,
  updated_at: now,
  stale: false,
  host: 'local',
}]))

async function render(quota: Record<string, unknown>, identities: unknown[], focusIdentity: string | null) {
  mockApi(sharedMock)
  resetStoreForTest()
  useStore.setState({ quota, identities } as never)
  await mount(<QuotaStrip focusKind={identities.some((id) => (id as { kind?: string }).kind === 'claude') ? 'claude' : 'codex'} focusIdentity={focusIdentity} />)
}

const focused = () => [...document.querySelectorAll<HTMLElement>('.quota-hp.focused')]

virtualMockTime()
afterEach(async () => {
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

it('codex bot 用 work 身分：只亮 codex:work 那一格', async () => {
  await render(quotaFor('codex', 'codex:work'), [
    { name: 'work', kind: 'codex', env: { CODEX_HOME: '/tmp/x' }, args: [], host: null },
  ], 'work')
  const lit = focused()
  assert.equal(lit.length, 1)
  assert.ok(lit[0].textContent?.includes('work'))
  assert.equal(document.querySelectorAll('[aria-current="true"]').length, 1)
})

it('codex bot 用預設帳號：只亮裸 codex 那一格', async () => {
  await render(quotaFor('codex', 'codex:work'), [
    { name: 'work', kind: 'codex', env: { CODEX_HOME: '/tmp/x' }, args: [], host: null },
  ], null)
  const lit = focused()
  assert.equal(lit.length, 1)
  assert.equal(lit[0].querySelector('.quota-identity')?.textContent, 'codex')
})

it('身分那一格沒有讀數時退回預設帳號那一格', async () => {
  await render(quotaFor('codex'), [
    { name: 'work', kind: 'codex', env: { CODEX_HOME: '/tmp/x' }, args: [], host: null },
  ], 'work')
  const lit = focused()
  assert.equal(lit.length, 1)
  assert.equal(lit[0].querySelector('.quota-identity')?.textContent, 'codex')
})

it('claude 仍只亮指定的 cc1 身分', async () => {
  await render(quotaFor('claude', 'claude:cc1'), [
    { name: 'cc0', kind: 'claude', env: {}, args: [], host: null },
    { name: 'cc1', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '/tmp/cc1' }, args: [], host: null },
  ], 'cc1')
  const lit = focused()
  assert.equal(lit.length, 1)
  assert.equal(lit[0].querySelector('.quota-identity')?.textContent, 'cc1')
})
