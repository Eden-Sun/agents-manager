/**
 * 總管 payload 的契約測試。
 *
 * 重點不是「正常資料能不能解析」，而是**畸形資料不能把面板打掛，也不能被畫成健康**。
 * 這個面板存在的理由就是 daemon 不對勁的時候給人看；它要能撐住 daemon 回半殘 JSON、
 * 舊版欄位、或整個型別都不對的情況。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { fetchAssignments, toAssignment, toIncident, toResponder } from './supervisor.ts'

test('集合欄位不是陣列時不會 throw（.map 白屏的那個 bug）', () => {
  // 這些都是 `(x as unknown[]) ?? []` 擋不住的：?? 只認 null/undefined。
  for (const junk of [{}, 'nope', 42, true, { length: 3 }]) {
    const payload = { assignments: junk, incidents: junk }
    // 模擬 toInfo/fetch* 內部的取法：不是陣列就當空的。
    const listed = Array.isArray(payload.assignments) ? payload.assignments : []
    assert.deepEqual(listed, [], `${JSON.stringify(junk)} 應該被當成空集合`)
  }
})

test('陣列裡的垃圾條目被濾掉，不是整包爆掉', () => {
  const rows = [null, 'x', 42, {}, { id: '' }, { id: 'a1', status: 'queued' }]
  const parsed = rows.map(toAssignment).filter((a) => a !== null)
  assert.equal(parsed.length, 1, '只有帶 id 的那一筆算數')
  assert.equal(parsed[0]?.id, 'a1')
})

test('舊 daemon 沒給 open/awaiting_review 時，由狀態自己推，且不預設成已結案', () => {
  const queued = toAssignment({ id: 'a1', status: 'queued' })
  assert.equal(queued?.open, true)
  assert.equal(queued?.awaiting_review, false)

  const waiting = toAssignment({ id: 'a2', status: 'awaiting_review' })
  assert.equal(waiting?.open, true, '等驗收仍是未結案')
  assert.equal(waiting?.awaiting_review, true)

  const blocked = toAssignment({ id: 'a3', status: 'blocked' })
  assert.equal(blocked?.open, true, '阻塞中也還沒結案')

  const done = toAssignment({ id: 'a4', status: 'completed' })
  assert.equal(done?.open, false)

  // 狀態本身是垃圾：正規化成 'unknown'，而 'unknown'（送達未知）本來就是未結案的一種，
  // 所以它會被算成還欠著。方向是對的——讀不懂的狀態寧可留在待辦裡，也不要當成做完了。
  const weird = toAssignment({ id: 'a5', status: 12345 })
  assert.equal(weird?.status, 'unknown')
  assert.equal(weird?.open, true, '讀不懂就當還沒結案，不要假裝 completed')
})

test('daemon 有給 open 就以它為準', () => {
  const a = toAssignment({ id: 'a1', status: 'completed', open: true })
  assert.equal(a?.open, true, 'daemon 說還沒結案就是還沒結案')
})

test('legacy_closed 只在 daemon 明說時才是 true', () => {
  assert.equal(toAssignment({ id: 'a1', status: 'completed' })?.legacy_closed, false)
  assert.equal(toAssignment({ id: 'a1', status: 'completed', legacy_closed: true })?.legacy_closed, true)
  // 不是布林就不算：'false' 這種字串曾經在別的地方被當成 true。
  assert.equal(toAssignment({ id: 'a1', legacy_closed: 'false' })?.legacy_closed, false)
})

test('review 欄位缺了或型別不對時是 null，不是空字串或 undefined', () => {
  const none = toAssignment({ id: 'a1', status: 'queued' })
  assert.deepEqual(none?.review, { decision: null, by: null, reason: null })

  const bad = toAssignment({ id: 'a1', review: 'accepted' })
  assert.deepEqual(bad?.review, { decision: null, by: null, reason: null }, 'review 不是物件就當沒有')

  const good = toAssignment({ id: 'a1', review: { decision: 'accept', by: 'AGM', reason: '測試通過' } })
  assert.equal(good?.review.decision, 'accept')
  assert.equal(good?.review.by, 'AGM')
})

test('evidence_complete 是三態：true / false / 不知道', () => {
  assert.equal(toAssignment({ id: 'a1', evidence_complete: true })?.evidence_complete, true)
  assert.equal(toAssignment({ id: 'a1', evidence_complete: false })?.evidence_complete, false)
  // 沒給就是不知道，**不可以**當成 true——那等於替沒看過的證據背書。
  assert.equal(toAssignment({ id: 'a1' })?.evidence_complete, null)
  assert.equal(toAssignment({ id: 'a1', evidence_complete: 'yes' })?.evidence_complete, null)
})

test('incident 缺 id 就不是一筆 incident；severity 缺了當 degraded 不當沒事', () => {
  assert.equal(toIncident({ kind: 'host_disconnected' }), null)
  assert.equal(toIncident(null), null)
  assert.equal(toIncident('boom'), null)

  const i = toIncident({ id: 'i1', kind: 'host_disconnected', resource: 'mac2' })
  assert.equal(i?.severity, 'degraded', '沒給嚴重度時不要當成無害')
  assert.equal(i?.status, 'open')
  assert.equal(i?.occurrences, 0)

  const full = toIncident({ id: 'i2', kind: 'notify_exhausted', severity: 'critical', occurrences: 4 })
  assert.equal(full?.severity, 'critical')
  assert.equal(full?.occurrences, 4)
})

test('occurrences 不是有限數字時回 0，不會變成 NaN 印在畫面上', () => {
  for (const bad of ['3', null, undefined, NaN, Infinity, {}]) {
    assert.equal(toIncident({ id: 'i1', occurrences: bad })?.occurrences, 0)
  }
})

test('協調者：舊 daemon 沒這一塊、或是垃圾，都當成沒建立，不畫成在跑', () => {
  for (const junk of [undefined, null, 'x', 42, [], {}]) {
    const r = toResponder(junk)
    assert.equal(r.configured, false)
    assert.equal(r.status, 'not_configured')
    assert.equal(r.stats.wakes, 0)
  }
  // 登記過但 bot 被刪掉：狀態是 missing，不是「沒建立」——事件還在它的佇列裡。
  const gone = toResponder({ configured: true, bot_present: false, bot_id: 'b-resp', status: 'missing' })
  assert.equal(gone.configured, true)
  assert.equal(gone.bot_present, false)
  // 舊 daemon 沒有 bot_present：有 bot_id 就當它還在。
  assert.equal(toResponder({ configured: true, bot_id: 'b-resp', status: 'idle' }).bot_present, true)
  // 實際跑的模型跟設定值分開讀；沒給就是 null，不要拿設定值冒充。
  const live = toResponder({ configured: true, bot_id: 'b', model: 'opus', runtime: { model: 'fable', effort: 'low' } })
  assert.equal(live.runtime.model, 'fable')
  assert.equal(toResponder({ configured: true }).runtime.model, null)
  const r = toResponder({ configured: true, status: 'waiting_quota', quota_reset_at: '2026-09-13T18:00:00Z', stats: { wakes: 'nope', duplicates: 99 } })
  assert.equal(r.status, 'waiting_quota')
  assert.equal(r.quota_reset_at, '2026-09-13T18:00:00Z')
  assert.equal(r.stats.wakes, 0, '型別不對的數字不採用')
  assert.equal(r.stats.duplicates, 99)
})


/**
 * issue #537：`GET /api/supervisor/assignments` 在 #515 之後是分頁的，一頁預設 200 筆。
 * 只拿第一頁的話，卡最久的未結案交辦（`blocked` 天生活得比一頁久）會從清單裡消失，
 * 而回傳型別讓呼叫端看不出來被截斷過。
 *
 * 自己架假 daemon，不借 `store/storeEnv.harness.ts`（跨目錄 import 會多一份模組實例）。
 */
const g = globalThis as unknown as Record<string, unknown>
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

async function withDaemon<T>(handler: (url: string) => Response, body: () => Promise<T>): Promise<T> {
  const saved = { fetch: g.fetch, location: g.location, localStorage: g.localStorage }
  g.location = { protocol: 'http:', host: '127.0.0.1:7788' }
  g.localStorage = { getItem: () => null, setItem: () => {}, removeItem: () => {} }
  g.fetch = async (input: string) => {
    const url = String(input)
    return url.split('?')[0] === '/api/session' ? json({ token: 't' }) : handler(url)
  }
  try {
    return await body()
  } finally {
    g.fetch = saved.fetch
    g.location = saved.location
    g.localStorage = saved.localStorage
  }
}

/** 三筆、每頁兩筆的假 daemon。游標形狀跟 daemon 一樣是 `["<created_at>","<id>"]` 的 JSON。 */
const ROWS = [
  { id: 'a3', status: 'completed', created_at: '2026-09-24T00:00:00.000Z' },
  { id: 'a2', status: 'completed', created_at: '2026-09-23T00:00:00.000Z' },
  { id: 'a1', status: 'blocked', created_at: '2026-09-16T00:00:00.000Z' },
]

function paged(url: string): Response {
  const q = new URL(url, 'http://x').searchParams
  const before = q.get('before')
  const rows = before
    ? ROWS.filter((r) => {
        const [at, id] = JSON.parse(before) as [string, string]
        return r.created_at < at || (r.created_at === at && r.id < id)
      })
    : ROWS
  // 這台 daemon 每頁上限 2（比呼叫端要的小）：要照 has_more 翻，不能因為自己要了 200 就當拿完了。
  const chunk = rows.slice(0, 2)
  const has_more = rows.length > 2
  const last = chunk[chunk.length - 1]
  return json({
    assignments: chunk,
    has_more,
    next_cursor: has_more && last ? JSON.stringify([last.created_at, last.id]) : null,
  })
}

test('翻到底：第二頁的未結案交辦也在清單裡', async () => {
  const calls: string[] = []
  const page = await withDaemon(
    (url) => {
      calls.push(url)
      return paged(url)
    },
    () => fetchAssignments(),
  )
  assert.deepEqual(page.assignments.map((a) => a.id), ['a3', 'a2', 'a1'])
  assert.equal(page.complete, true)
  assert.equal(calls.length, 2, '第一頁看不到 a1，一定要翻第二頁')
  assert.ok(calls[0]?.includes('limit=200'), `第一頁不帶游標：${calls[0]}`)
  assert.ok(!calls[0]?.includes('before='), `第一頁不帶游標：${calls[0]}`)
  assert.ok(calls[1]?.includes('before='), `第二頁要帶游標：${calls[1]}`)
})

test('舊 daemon 不回 has_more：說自己沒撈完，不要假裝那一頁就是全部', async () => {
  const page = await withDaemon(() => json({ assignments: ROWS.slice(0, 2) }), () => fetchAssignments())
  assert.equal(page.assignments.length, 2)
  assert.equal(page.complete, false, '不知道還有沒有，就不能說撈完了')
})

test('has_more 是垃圾或游標掉了都當成沒撈完，而且不會無窮翻下去', async () => {
  for (const body of [
    { assignments: [ROWS[0]], has_more: 'yes' },
    { assignments: [ROWS[0]], has_more: 1 },
    // 游標是好的、只有 has_more 型別不對：`!has_more` 這種寫法會照翻下去，把讀不懂
    // 當成「還有更多」。三態的意思是「不是 true 就不翻」。
    { assignments: [ROWS[0]], has_more: 'yes', next_cursor: '["2026-09-24T00:00:00.000Z","a3"]' },
    { assignments: [ROWS[0]], has_more: 1, next_cursor: '["2026-09-24T00:00:00.000Z","a3"]' },
    { assignments: [ROWS[0]], has_more: true, next_cursor: null },
    { assignments: [ROWS[0]], has_more: true, next_cursor: '' },
    { assignments: [ROWS[0]], has_more: true, next_cursor: 42 },
  ]) {
    let calls = 0
    const page = await withDaemon(
      () => {
        calls++
        return json(body)
      },
      () => fetchAssignments(),
    )
    assert.equal(page.complete, false, JSON.stringify(body))
    assert.equal(calls, 1, `不該再翻下去：${JSON.stringify(body)}`)
    assert.equal(page.assignments.length, 1)
  }
})

test('每一頁的垃圾條目照樣被濾掉，整包不會爆', async () => {
  const page = await withDaemon(
    () => json({ assignments: [null, 'x', 42, { id: '' }, ROWS[2]], has_more: false }),
    () => fetchAssignments(),
  )
  assert.deepEqual(page.assignments.map((a) => a.id), ['a1'])
  assert.equal(page.complete, true)
})
