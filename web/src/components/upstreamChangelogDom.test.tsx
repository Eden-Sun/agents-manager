/**
 * agy、grok 更新框的 changelog 與分析（#1930）。真的掛進 happy-dom：upstream 快照走真的 `parseUpstreamItem`，
 * 後端是模擬的 `/api/changelog`（還沒認得 agy／grok 時回 `found:false`＋claude 的連結）與 400 的 review／triage。
 * claude 同樣讀不到分析時整塊不畫，維持原樣。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { setupDom, teardownDom, unmountAll, mount, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { parseUpstreamItem } from '../store/upstreamUpdate'
import { AgyUpdateChip } from './AgyUpdateChip'
import { GrokUpdateChip } from './GrokUpdateChip'
import { AgmReviewBox } from './AgmReviewBox'
import { changelogRange } from '../lib/upstreamRange'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const AGY_RAW = 'https://raw.githubusercontent.com/google-antigravity/antigravity-cli/refs/heads/main/CHANGELOG.md'
const AGY_RELEASES = 'https://github.com/google-antigravity/antigravity-cli/releases'
const control = { open: true, close: () => {} }

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

/** 只有 `/changelog` 有東西可回；review／triage 一律 400（後端還不收這個 kind），翻譯沒開。 */
function backend(changelog: unknown): string[] {
  const seen: string[] = []
  globalThis.fetch = (async (input: string) => {
    const path = String(input)
    seen.push(path)
    if (path.includes('/changelog/translation')) return json({ enabled: false, status: 'unconfigured', error: null, summary: [], sections: [] })
    if (path.includes('/changelog?')) return json(changelog)
    if (path.includes('/claude-update/review') || path.includes('/release-triage')) return json({ error: 'kind 不支援' }, 400)
    return json({})
  }) as unknown as typeof fetch
  return seen
}

const notFound = (kind: string) => ({
  kind,
  host: 'local',
  installed_version: null,
  from_version: null,
  found: false,
  sections: [],
  source_url: 'https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md',
  error: `${kind} 沒有 changelog 來源`,
})

function setUpstream(kind: 'agy' | 'grok', target: string, hosts: [string, string | null][]) {
  const item = parseUpstreamItem({
    kind,
    latest_version: target,
    target_version: target,
    has_update: true,
    hosts: hosts.map(([host, v]) => ({ host, installed_version: v, behind: true })),
  })
  useStore.setState({ upstreamUpdates: { [kind]: item! } })
}

const changelogQuery = (seen: string[]) => new URL(seen.find((p) => p.includes('/changelog?'))!, 'http://local').searchParams

test('agy：後端回 found:false 時寫「找不到這一版的更新說明」＋官方連結（不是 claude 的），分析區寫沒有分析', async () => {
  const seen = backend(notFound('agy'))
  setUpstream('agy', '1.4.6', [['local', '1.4.5']])
  const root = await mount(<AgyUpdateChip control={control} />)
  await until(() => document.body.textContent?.includes('找不到這一版的更新說明') === true, '找不到')
  await until(() => document.body.textContent?.includes('這個版本沒有分析') === true, '沒有分析')
  const hrefs = [...document.body.querySelectorAll('a')].map((a) => a.getAttribute('href'))
  assert.ok(hrefs.includes(AGY_RAW), String(hrefs))
  assert.ok(hrefs.includes(AGY_RELEASES), '備援連結')
  assert.equal(hrefs.some((h) => h?.includes('anthropics')), false, '不能指到 claude')
  const q = changelogQuery(seen)
  assert.equal(q.get('kind'), 'agy')
  assert.equal(q.get('to'), '1.4.6')
  assert.equal(q.get('from'), '1.4.5')
  assert.ok(document.body.textContent?.includes('驗 sha512'), '原本的安裝說明還在')
  void root
})

test('agy：後端有 changelog 時畫出條目', async () => {
  backend({
    kind: 'agy',
    host: 'local',
    installed_version: '1.4.6',
    from_version: '1.4.5',
    found: true,
    sections: [{ version: '1.4.6', body: '- Added a `Configure MCP servers...` row' }],
    source_url: AGY_RAW,
    error: null,
  })
  setUpstream('agy', '1.4.6', [['local', '1.4.5']])
  await mount(<AgyUpdateChip control={control} />)
  await until(() => document.body.textContent?.includes('Configure MCP servers') === true, '條目')
  assert.equal(document.body.textContent?.includes('找不到這一版'), false)
})

test('agy 多台落後：取最舊那台當 from；任何一台版本未知就不傳 from', async () => {
  const seen = backend(notFound('agy'))
  setUpstream('agy', '1.4.6', [['a', '1.4.5'], ['b', '1.4.4']])
  await mount(<AgyUpdateChip control={control} />)
  await until(() => seen.some((p) => p.includes('/changelog?')), '請求')
  assert.equal(changelogQuery(seen).get('from'), '1.4.4')
  await unmountAll()

  const seen2 = backend(notFound('agy'))
  setUpstream('agy', '1.4.6', [['a', '1.4.5'], ['b', null]])
  await mount(<AgyUpdateChip control={control} />)
  await until(() => seen2.some((p) => p.includes('/changelog?')), '請求')
  assert.equal(changelogQuery(seen2).has('from'), false)
})

test('grok：連結指到這一版那份 CDN 檔，不是 claude 的', async () => {
  const seen = backend(notFound('grok'))
  setUpstream('grok', '1.0.67', [['local', '1.0.66']])
  await mount(<GrokUpdateChip control={control} />)
  await until(() => document.body.textContent?.includes('找不到這一版的更新說明') === true, '找不到')
  await until(() => document.body.textContent?.includes('這個版本沒有分析') === true, '沒有分析')
  const hrefs = [...document.body.querySelectorAll('a')].map((a) => a.getAttribute('href'))
  assert.ok(hrefs.includes('https://storage.googleapis.com/grok-build-public-artifacts/cli/changelogs/1.0.67.external.md'), String(hrefs))
  assert.equal(changelogQuery(seen).get('kind'), 'grok')
  assert.ok(document.body.textContent?.includes('grok update'), '原本的升級指令還在')
})

test('claude 兩路都讀不到分析：整塊不畫，維持原樣', async () => {
  backend({ ...notFound('claude'), found: false })
  const root = await mount(<AgmReviewBox kind="claude" host="local" from="2.1.3" to="2.1.5" refreshKey={0} />)
  await new Promise((r) => setTimeout(r, 100))
  assert.equal(root.querySelector('.agm-review'), null)
  assert.equal(root.textContent?.includes('這個版本沒有分析'), false)
})

test('codex、herdr 也維持原樣；只有 agy、grok 寫沒有分析', async () => {
  backend({})
  for (const kind of ['codex', 'herdr']) {
    const root = await mount(<AgmReviewBox kind={kind} host="local" from="0.1.0" to="0.2.0" refreshKey={0} />)
    await new Promise((r) => setTimeout(r, 100))
    assert.equal(root.querySelector('.agm-review'), null, kind)
    await unmountAll()
  }
  const root = await mount(<AgmReviewBox kind="agy" host="local" from="1.4.5" to="1.4.6" refreshKey={0} />)
  await until(() => root.querySelector('.agm-review') !== null, 'agy 有說明')
  assert.match(root.textContent ?? '', /這個版本沒有分析/)
})

// 區間起點的判斷（純函式，跟 DOM 測試同一檔，避免多一個測試檔就把 shard 切點挪走）。
test('多台落後：取最舊那台當區間起點', () => {
  assert.deepEqual(
    changelogRange([
      { host: 'a', from: '1.4.5' },
      { host: 'b', from: '1.4.4' },
      { host: 'c', from: '1.10.0' },
    ]),
    { host: 'b', from: '1.4.4' },
  )
})

test('數值比較，不是字串比較（1.4.10 比 1.4.9 新）', () => {
  assert.deepEqual(changelogRange([{ host: 'a', from: '1.4.10' }, { host: 'b', from: '1.4.9' }]), { host: 'b', from: '1.4.9' })
})

test('任何一台版本未知就傳 null，不猜', () => {
  assert.deepEqual(changelogRange([{ host: 'a', from: '1.4.5' }, { host: 'b', from: null }]), { host: 'a', from: null })
  assert.deepEqual(changelogRange([{ host: 'b', from: null }, { host: 'a', from: '1.4.5' }]), { host: 'b', from: null })
})

test('不是純數字版本（讀到一整行）也當未知', () => {
  assert.equal(changelogRange([{ host: 'a', from: 'agy 1.4.5' }]).from, null)
  assert.equal(changelogRange([{ host: 'a', from: '1.4.5-beta' }, { host: 'b', from: '1.4.4' }]).from, null)
})

test('沒有主機退回 local、null', () => {
  assert.deepEqual(changelogRange([]), { host: 'local', from: null })
})
