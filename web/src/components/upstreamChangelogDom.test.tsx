/**
 * agy、grok 更新框的 changelog 與分析（#1930）。真的掛進 happy-dom：upstream 快照走真的 `parseUpstreamItem`，
 * 後端是模擬的 `/api/changelog`（還沒認得 agy／grok 時回 `found:false`＋claude 的連結）；review／triage 預設 400（舊 daemon）。
 * 分析區跟 claude／codex／herdr 一樣：兩邊都讀不到整塊不畫；讀得到但沒分析寫「尚未分析——按下方「分析影響」」，
 * 確認框底有「分析影響」，只有按下去才會 POST（開框只讀，#1928）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { setupDom, teardownDom, unmountAll, mount, click, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { parseUpstreamItem } from '../store/upstreamUpdate'
import { AgyUpdateChip } from './AgyUpdateChip'
import { GrokUpdateChip } from './GrokUpdateChip'
import { AgmReviewBox } from './AgmReviewBox'
import { changelogRange } from '../lib/upstreamRange'
import { useReviewAsk } from '../hooks/useReviewAsk'

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

/** 收到的非 GET 請求（方法與路徑）：開框只讀，按「分析影響」才會有 POST。 */
let writes: { method: string; path: string; body: Record<string, unknown> }[] = []

/**
 * 只有 `/changelog` 有東西可回；翻譯沒開。`reviewable=false`（預設）：review／triage 一律 400（舊 daemon 不收這個 kind）；
 * `true`：帳本讀得到但是空的（沒分析過），review 讀得到、POST 派得出去。
 */
function backend(changelog: unknown, reviewable = false): string[] {
  const seen: string[] = []
  writes = []
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const path = String(input)
    const method = init?.method ?? 'GET'
    seen.push(path)
    if (method !== 'GET') writes.push({ method, path, body: JSON.parse(String(init?.body ?? '{}')) as Record<string, unknown> })
    if (path.includes('/changelog/translation')) return json({ enabled: false, status: 'unconfigured', error: null, summary: [], sections: [] })
    if (path.includes('/changelog?')) return json(changelog)
    if (path.includes('/claude-update/review')) {
      if (!reviewable) return json({ error: 'kind 不支援' }, 400)
      return method === 'GET'
        ? json({ kind: 'agy', version: '1.4.6', review: { state: 'none' } })
        : json({ kind: 'agy', version: '1.4.6', target_bot_name: 'AGM-responder', duplicate: false, review: { state: 'pending' } })
    }
    if (path.includes('/release-triage')) return reviewable ? json({ publish_enabled: false, repo: '', rows: [] }) : json({ error: 'kind 不支援' }, 400)
    return json({})
  }) as unknown as typeof fetch
  return seen
}

const written = () => writes
const idle = (ms = 150) => new Promise((r) => setTimeout(r, ms))
const reviewBtn = () => [...document.body.querySelectorAll('button')].find((b) => b.textContent?.includes('分析影響'))

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

test('agy：後端回 found:false 時寫「找不到這一版的更新說明」＋官方連結（不是 claude 的）；分析兩邊都讀不到就不畫、但有「分析影響」', async () => {
  const seen = backend(notFound('agy'))
  setUpstream('agy', '1.4.6', [['local', '1.4.5']])
  const root = await mount(<AgyUpdateChip control={control} />)
  await until(() => document.body.textContent?.includes('找不到這一版的更新說明') === true, '找不到')
  await idle()
  assert.equal(document.body.querySelector('.agm-review'), null, '同 claude：讀不到分析整塊不畫')
  assert.ok(reviewBtn(), '確認框有「分析影響」')
  assert.deepEqual(written(), [], '開框只讀，不能 POST')
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
  await idle()
  assert.equal(document.body.querySelector('.agm-review'), null)
  assert.ok(reviewBtn(), 'grok 確認框也有「分析影響」')
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

test('claude、codex、herdr、agy、grok 兩路都讀不到分析：一律整塊不畫（agy、grok 不再有專屬的「沒有分析」）', async () => {
  backend({})
  for (const kind of ['claude', 'codex', 'herdr', 'agy', 'grok']) {
    const root = await mount(<AgmReviewBox kind={kind} host="local" from="0.1.0" to="0.2.0" refreshKey={0} />)
    await idle(100)
    assert.equal(root.querySelector('.agm-review'), null, kind)
    assert.equal(root.textContent?.includes('沒有分析'), false, kind)
    await unmountAll()
  }
})

test('讀得到但沒分析過：五個 kind 同一句「尚未分析——按下方「分析影響」，結論會回到這裡。」', async () => {
  backend({}, true)
  const want = '尚未分析——按下方「分析影響」，結論會回到這裡。'
  for (const kind of ['claude', 'codex', 'herdr', 'agy', 'grok']) {
    const root = await mount(<AgmReviewBox kind={kind} host="local" from="1.4.5" to="1.4.6" refreshKey={0} />)
    await until(() => root.querySelector('.agm-review-none') !== null, `${kind} 尚未分析`)
    assert.equal(root.querySelector('.agm-review-none')?.textContent, want, kind)
    await unmountAll()
  }
})

test('agy：只有按「分析影響」才 POST（kind、from、to 帶對），派出去後分析區重讀；開框不 POST', async () => {
  const seen = backend({ ...notFound('agy'), found: false }, true)
  setUpstream('agy', '1.4.6', [['local', '1.4.5']])
  await mount(<AgyUpdateChip control={control} />)
  await until(() => document.body.querySelector('.agm-review-none') !== null, '尚未分析')
  assert.deepEqual(written(), [], '開框只讀')
  const reviewReads = () => seen.filter((p) => p.includes('/claude-update/review')).length
  const before = reviewReads()
  await click(reviewBtn()!)
  await until(() => written().length === 1, 'POST')
  assert.equal(written()[0].method, 'POST')
  assert.match(written()[0].path, /\/claude-update\/review$/)
  assert.deepEqual({ kind: written()[0].body.kind, from: written()[0].body.from, to: written()[0].body.to }, { kind: 'agy', from: '1.4.5', to: '1.4.6' })
  await until(() => reviewReads() > before + 1, '派出去後分析區重讀（refreshKey）')
  assert.equal(written().length, 1, '只 POST 一次')
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

test('API 分析失敗顯示模型、原因及重試；翻譯與分析 POST 分離', async () => {
  const posts: string[] = []
  let state = 'failed'
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const path = String(input)
    const method = init?.method ?? 'GET'
    if (method === 'POST') {
      posts.push(path)
      state = 'pending'
    }
    if (path.includes('/claude-update/review')) return json({
      version: '1.4.6', review: { state, source: 'api', model: 'claude-sonnet-test', error: 'bad_format:summary' },
    })
    return json({ rows: [], repo: '' })
  }) as unknown as typeof fetch
  const root = await mount(<AgmReviewBox kind="agy" host="local" from="1.4.5" to="1.4.6" refreshKey={0} />)
  await until(() => root.textContent?.includes('bad_format:summary') === true, '失敗原因')
  assert.match(root.textContent ?? '', /影響分析/)
  assert.match(root.textContent ?? '', /Claude API（claude-sonnet-test）/)
  assert.equal(posts.length, 0, 'GET 不付費')
  await click([...root.querySelectorAll('button')].find((b) => b.textContent === '重試')!)
  await until(() => root.textContent?.includes('分析中，') === true, '重試後 pending')
  assert.equal(posts.length, 1)
  assert.match(posts[0], /claude-update\/review/)
  assert.doesNotMatch(posts[0], /translation/)
})

test('逐條 API 失敗顯示原因並只重試指定版本', async () => {
  const writes: Record<string, unknown>[] = []
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const path = String(input)
    if (init?.method === 'POST') writes.push(JSON.parse(String(init.body)))
    if (path.includes('/claude-update/review')) return json({ version: '1.4.6', review: { state: 'none' } })
    return json({ rows: [{ version: '1.4.6', status: 'failed', entries: [], verdicts: { analysis: {
      source: 'api', model: 'claude-sonnet-test', error: 'issue title 不能是空的',
    } } }] })
  }) as unknown as typeof fetch
  const root = await mount(<AgmReviewBox kind="agy" host="local" from="1.4.5" to="1.4.6" refreshKey={0} />)
  await until(() => root.textContent?.includes('issue title 不能是空的') === true, '逐條失敗原因')
  assert.match(root.textContent ?? '', /Claude API（claude-sonnet-test）/)
  await click([...root.querySelectorAll('button')].find((b) => b.textContent === '重試')!)
  await until(() => writes.length === 1, '逐條重試')
  assert.deepEqual(writes, [{ kind: 'agy', versions: ['1.4.6'] }])
})

test('分析通知沿用 API 實際來源，不把成功快取說成 AGM 派工', async () => {
  useStore.setState({ notices: [] })
  globalThis.fetch = (async () => json({
    version: '1.4.6', duplicate: true,
    review: { state: 'done', source: 'api', model: 'claude-sonnet-test', result: '結論' },
  })) as unknown as typeof fetch
  function Probe() {
    const { dialog } = useReviewAsk('agy', 'local', '1.4.5', '1.4.6')
    return <button type="button" onClick={dialog.onSecondary}>{dialog.secondaryLabel}</button>
  }
  const root = await mount(<Probe />)
  await click(root.querySelector('button')!)
  await until(() => useStore.getState().notices.length > 0, '分析來源通知')
  const text = useStore.getState().notices.at(-1)!.text
  assert.match(text, /Claude API（claude-sonnet-test）/)
  assert.doesNotMatch(text, /AGM/)
})
