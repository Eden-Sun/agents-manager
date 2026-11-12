/**
 * 額度用完的重置券提問（真的掛進 happy-dom）：
 * - 有券＋用完 → 跳一次提問；「不用」關掉，同一次耗盡不再跳；
 * - 「使用一張」→ 打 daemon 的 User-only 端點，成功後關掉。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { startResetCreditOfferNotice } from '../store/resetCreditOffer'
import { ResetCreditOffer } from './ResetCreditOffer'
import type { QuotaMap } from '../api/types'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const NOW = Date.parse('2026-11-08T06:30:00Z')
const quotaWithHit = (at: string): QuotaMap =>
  ({
    'mac_builder/codex:co0': {
      five_hour: null,
      seven_day: null,
      fable: null,
      reset_credits: { available: 2, title: 'Full reset', expires_at: '2026-11-30T00:00:00Z', credits: [] },
      limit_hit: { message: 'limit', until: '2026-11-08T10:00:00Z', at, bucket: null },
      plan: null,
      updated_at: '',
      source: 'codex-app-server',
      account: null,
      host: 'mac_builder',
      stale: false,
    },
  }) as unknown as QuotaMap

const flush = () => new Promise((r) => setTimeout(r, 0))

test('用完＋有券：跳一次；按「不用」關掉，同一次耗盡不再跳', async () => {
  useStore.setState({ quota: quotaWithHit('2026-11-08T06:00:00Z') })
  const stop = startResetCreditOfferNotice(() => NOW)
  await mount(<ResetCreditOffer />)
  assert.match(document.body.textContent ?? '', /還有 2 張重置券/)
  const no = [...document.querySelectorAll('button')].find((b) => b.textContent?.trim() === '不用')!
  await click(no)
  await flush()
  assert.equal(document.querySelector('[role=alertdialog]'), null, '按不用就關')
  stop()

  // 同一次耗盡（同一個撞限紀錄）再來一次報價：不再跳
  const stop2 = startResetCreditOfferNotice(() => NOW + 60_000)
  useStore.setState({ quota: { ...quotaWithHit('2026-11-08T06:00:00Z') } })
  await flush()
  assert.equal(document.querySelector('[role=alertdialog]'), null)
  stop2()
})

test('「使用一張」：打 User-only 端點（不帶 credit_id），成功後關掉', async () => {
  useStore.setState({ quota: quotaWithHit('2026-11-08T07:00:00Z') })
  const calls: { url: string; body: unknown }[] = []
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(input), body: init?.body ? JSON.parse(String(init.body)) : undefined })
    return new Response(JSON.stringify({ outcome: 'reset', key: 'mac_builder/codex:co0', resumed: [] }), { status: 200, headers: { 'content-type': 'application/json' } })
  }) as typeof fetch
  try {
    const stop = startResetCreditOfferNotice(() => NOW)
    await mount(<ResetCreditOffer />)
    const use = [...document.querySelectorAll('button')].find((b) => b.textContent?.trim() === '使用一張')!
    await click(use)
    await flush()
    await flush()
    const hit = calls.find((c) => c.url.includes('/quota/reset-credit/use'))
    assert.ok(hit, `expected the use endpoint, got ${calls.map((c) => c.url).join(', ')}`)
    assert.deepEqual(hit.body, { host: 'mac_builder', identity: 'co0' })
    assert.equal(document.querySelector('[role=alertdialog]'), null)
    stop()
  } finally {
    globalThis.fetch = realFetch
  }
})
