import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import { resetClaudeApiBalanceForTest } from '../hooks/useClaudeApiBalance'
import { resetOpenAiApiBalanceForTest } from '../hooks/useOpenAiApiBalance'
import { BalanceQuotaChip } from './BalanceQuotaChip'
import { OpenAiBalanceChip } from './OpenAiBalanceChip'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetClaudeApiBalanceForTest()
  resetOpenAiApiBalanceForTest()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const settle = () => act(async () => { await new Promise((r) => setTimeout(r, 30)) })
const chip = () => document.querySelector<HTMLElement>('[data-k="openai-balance"]')

async function withBalances<T>(openai: unknown, fn: (puts: unknown[]) => Promise<T>, claude: unknown = { configured: false }): Promise<T> {
  const orig = rawTransport.request.bind(rawTransport)
  const puts: unknown[] = []
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path === '/openai-api/balance') return openai
    if (path === '/claude-api/balance') return claude
    if (path === '/openai-api/balance-anchor' && method === 'PUT') {
      puts.push(body)
      return { ok: true }
    }
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  try {
    return await fn(puts)
  } finally {
    rawTransport.request = orig
  }
}

const anchored = {
  enabled: true,
  configured: true,
  anchor: { usd: 20, at: '2026-11-10T00:00:00.000Z' },
  spent_usd: 2,
  remaining_usd: 18,
  uncertain_usd: 0,
  remaining_floor_usd: 18,
  as_of: '2026-11-19T01:00:00.000Z',
}

test('未啟用：整格不畫', async () => {
  await withBalances({ enabled: false, configured: false, as_of: '2026-11-19T01:00:00.000Z' }, async () => {
    await mount(<OpenAiBalanceChip />)
    await settle()
    assert.equal(chip(), null)
  })
})

test('啟用但沒有錨點：畫「設定 OpenAI 餘額」，點開的視窗說明要輸入餘額', async () => {
  await withBalances({ enabled: true, configured: false, as_of: '2026-11-19T01:00:00.000Z' }, async () => {
    await mount(<OpenAiBalanceChip />)
    await settle()
    assert.ok(chip(), '啟用就要有入口')
    assert.equal(chip()!.textContent, '設定 OpenAI 餘額')
    await click(chip()!)
    const dialog = document.querySelector<HTMLElement>('[data-k="openai-balance-dialog"]')
    assert.ok(dialog, '點開同一個設定視窗')
    assert.match(dialog!.textContent ?? '', /還沒設定餘額錨點/)
    assert.ok(document.querySelector('[data-k="openai-anchor-save"]'))
  })
})

test('有錨點：畫 OpenAI 字樣與金額；tooltip 標示估算與錨點', async () => {
  await withBalances(anchored, async () => {
    await mount(<OpenAiBalanceChip />)
    await settle()
    assert.deepEqual([...chip()!.children].map((c) => [c.className, c.textContent]), [['kind-glyph codex', ''], ['quota-balance-v', '$18.0']], 'GPT 圖示（無文字標籤）＋數字')
    assert.equal(chip()!.querySelector('.kind-glyph.codex')?.getAttribute('aria-label'), 'OpenAI API 餘額')
    assert.ok(chip()!.querySelector('.kind-glyph.codex svg'), '有 svg 圖示')
    const tip = chip()!.getAttribute('title') ?? ''
    assert.match(tip, /估算/)
    assert.match(tip, /錨點：\$20\.00/)
    assert.match(tip, /已確定花費：\$2\.00/)
  })
})

test('位置：OpenAI 晶片在 Claude 晶片之後', async () => {
  const claude = { configured: true, anchor: { usd: 98.14, at: '2026-11-02T05:52:10.209Z' }, spent_usd: 74.85, remaining_usd: 23.29, as_of: '2026-11-15T09:30:00.000Z', uncertain_usd: 0, remaining_floor_usd: 23.29, estimate: true }
  await withBalances(anchored, async () => {
    await mount(
      <>
        <BalanceQuotaChip />
        <OpenAiBalanceChip />
      </>,
    )
    await settle()
    const claudeChip = document.querySelector('[data-k="quota-balance"]')
    const openai = chip()
    assert.ok(claudeChip && openai)
    assert.ok(claudeChip!.compareDocumentPosition(openai!) & Node.DOCUMENT_POSITION_FOLLOWING, 'OpenAI 在 Claude 之後')
  }, claude)
})

test('點開校正：輸入金額送出 PUT balance-anchor，成功後關掉視窗', async () => {
  useStore.setState({ bots: [] } as never)
  await withBalances(anchored, async (puts) => {
    await mount(<OpenAiBalanceChip />)
    await settle()
    await click(chip()!)
    const input = document.querySelector<HTMLInputElement>('[data-k="openai-balance-dialog"] input[type="number"]')!
    const save = document.querySelector<HTMLButtonElement>('[data-k="openai-anchor-save"]')!
    assert.equal(save.disabled, true, '沒填不能送')
    await typeInto(input, '25.5')
    await click(save)
    await settle()
    assert.deepEqual(puts, [{ usd: 25.5, at: undefined }])
  })
})

const dayFromNow = (d: number) => new Date(Date.now() + d * 86_400_000).toISOString().slice(0, 10)
const withConsole = (console: unknown) => ({ ...anchored, console })

test('Console 實際值：畫 total_available，tooltip 有讀取時間、org、每筆 grant；7 天內到期加警示', async () => {
  const soon = dayFromNow(3)
  const c = {
    status: 'ok',
    total_available_usd: 82.5,
    total_granted_usd: 120,
    total_used_usd: 37.5,
    grants: [{ remaining_usd: 70, total_usd: 100, expires_on: soon }, { remaining_usd: 12.5, total_usd: 20, expires_on: null }],
    org_title: 'Biz',
    fetched_at: '2026-11-27T02:05:00.000Z',
  }
  await withBalances(withConsole(c), async () => {
    await mount(<OpenAiBalanceChip />)
    await settle()
    assert.equal(chip()!.querySelector('.quota-balance-v')?.textContent, '$82.5')
    assert.match(chip()!.className, /\bexact\b/)
    assert.match(chip()!.className, /\bwarn\b/, '3 天內到期要警示')
    const tip = chip()!.getAttribute('title') ?? ''
    assert.match(tip, /Console 實際（10:05 讀取）/)
    assert.match(tip, /組織：Biz/)
    assert.match(tip, new RegExp(`額度 \\$70\\.00 / \\$100\\.00，到期 ${soon}`))
    assert.match(tip, /額度 \$12\.50 \/ \$20\.00，到期 —/)
    assert.match(tip, new RegExp(`⚠ 額度 ${soon} 到期`))
    assert.doesNotMatch(tip, /估算/)
  })
  await unmountAll()
  resetOpenAiApiBalanceForTest()
  await withBalances(withConsole({ ...c, grants: [{ remaining_usd: 70, total_usd: 100, expires_on: dayFromNow(30) }] }), async () => {
    await mount(<OpenAiBalanceChip />)
    await settle()
    assert.doesNotMatch(chip()!.className, /\bwarn\b/, '30 天後到期不警示')
  })
})

test('登入過期：退回推算值並提示重新匯出', async () => {
  await withBalances(withConsole({ status: 'logged_out', fetched_at: '2026-11-27T02:05:00.000Z', error: 'login_rejected' }), async () => {
    await mount(<OpenAiBalanceChip />)
    await settle()
    assert.equal(chip()!.querySelector('.quota-balance-v')?.textContent, '$18.0')
    assert.match(chip()!.className, /\bestimate\b.*\bwarn\b/)
    const tip = chip()!.getAttribute('title') ?? ''
    assert.match(tip, /OpenAI 登入已過期，需重新匯出/)
    assert.match(tip, /估算/)
  })
})
