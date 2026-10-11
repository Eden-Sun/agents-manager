import { test } from 'node:test'
import assert from 'node:assert/strict'
import type { JudgeShadowRow } from '../api/judge'
import { botLabel, overThreshold, probabilityText, recentRows, sceneStats } from './judgeStats'

const NOW = Date.parse('2026-10-11T12:00:00Z')
const ago = (h: number) => new Date(NOW - h * 3600_000).toISOString()

function row(p: Partial<JudgeShadowRow> & Pick<JudgeShadowRow, 'regex_verdict' | 'at'>): JudgeShadowRow {
  return {
    id: `r-${Math.random().toString(36).slice(2, 8)}`,
    bot_id: null,
    run_id: null,
    kind: null,
    matched_line: null,
    composer_idle: null,
    jev_is_live_ui: null,
    jev_same_work: null,
    model: null,
    ms: null,
    input_tokens: null,
    error: null,
    cleared_at: null,
    assignment_id: null,
    claims_verified: null,
    asks_parent_action: null,
    ...p,
  }
}

test('超過門檻照 SPEC：stuck ≥0.7、same_work ≥0.5、report_evidence 的 claims <0.8 或 asks ≥0.8；沒證據不算超過', () => {
  assert.equal(overThreshold(row({ regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.7 })), true)
  assert.equal(overThreshold(row({ regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.69 })), false)
  assert.equal(overThreshold(row({ regex_verdict: 'stuck_queued', at: ago(1) })), false, '沒有證據不算超過')
  assert.equal(overThreshold(row({ regex_verdict: 'same_work', at: ago(1), jev_same_work: 0.5 })), true)
  assert.equal(overThreshold(row({ regex_verdict: 'same_work', at: ago(1), jev_same_work: 0.49 })), false)
  assert.equal(overThreshold(row({ regex_verdict: 'report_evidence', at: ago(1), claims_verified: 0.79 })), true)
  assert.equal(overThreshold(row({ regex_verdict: 'report_evidence', at: ago(1), claims_verified: 0.8, asks_parent_action: 0.8 })), true)
  assert.equal(overThreshold(row({ regex_verdict: 'report_evidence', at: ago(1), claims_verified: 0.95, asks_parent_action: 0.2 })), false)
})

test('撞限第二意見沒有門檻：超過欄位是 null，不假裝有門檻', () => {
  assert.equal(overThreshold(row({ regex_verdict: 'limit_hit', at: ago(1), jev_is_live_ui: 0.99 })), null)
  const [limit] = sceneStats([row({ regex_verdict: 'limit_hit', at: ago(1), jev_is_live_ui: 0.99 })], NOW)
  assert.equal(limit!.windows.day.over, null)
  assert.equal(limit!.windows.day.total, 1)
})

test('三個時間窗：近 24 小時、近 7 天、全部；超出 7 天的只算全部', () => {
  const rows = [
    row({ regex_verdict: 'stuck_queued', at: ago(2), jev_is_live_ui: 0.9 }),
    row({ regex_verdict: 'stuck_queued', at: ago(30), jev_is_live_ui: 0.9 }),
    row({ regex_verdict: 'stuck_queued', at: ago(24 * 10), jev_is_live_ui: 0.1 }),
  ]
  const [, stuck] = sceneStats(rows, NOW)
  assert.deepEqual(
    { day: stuck!.windows.day, week: stuck!.windows.week, all: stuck!.windows.all },
    {
      day: { total: 1, over: 1, errors: 0 },
      week: { total: 2, over: 2, errors: 0 },
      all: { total: 3, over: 2, errors: 0 },
    },
  )
})

test('錯誤筆數與最後一次時間：空字串不算錯誤；最後一次取時間最新的那列', () => {
  const rows = [
    row({ regex_verdict: 'same_work', at: ago(5), error: 'timeout' }),
    row({ regex_verdict: 'same_work', at: ago(1), error: '' }),
    row({ regex_verdict: 'same_work', at: ago(3), jev_same_work: 0.6 }),
  ]
  const [, , same] = sceneStats(rows, NOW)
  assert.equal(same!.windows.day.errors, 1, '5 小時前的錯誤在近 24 小時窗內')
  assert.equal(same!.windows.all.errors, 1)
  assert.equal(same!.last, ago(1), '最後一次是 1 小時前那列')
})

test('沒有紀錄：總數為 0，最後一次是 null，四個場景都有一列', () => {
  const stats = sceneStats([], NOW)
  assert.equal(stats.length, 4)
  assert.ok(stats.every((s) => s.last === null && s.windows.all.total === 0))
})

test('最近清單新的在前；時間解不開的排最後', () => {
  const rows = [
    row({ regex_verdict: 'same_work', at: ago(3) }),
    row({ regex_verdict: 'same_work', at: 'not-a-date' }),
    row({ regex_verdict: 'same_work', at: ago(1) }),
  ]
  assert.deepEqual(recentRows(rows, 2).map((r) => r.at), [ago(1), ago(3)])
  assert.equal(recentRows(rows).at(-1)!.at, 'not-a-date')
})

test('機率欄照場景取欄位；報告證據兩個都帶', () => {
  assert.equal(probabilityText(row({ regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.8 })), '0.80')
  assert.equal(probabilityText(row({ regex_verdict: 'same_work', at: ago(1), jev_same_work: 0.5 })), '0.50')
  assert.equal(probabilityText(row({ regex_verdict: 'report_evidence', at: ago(1), claims_verified: 0.9, asks_parent_action: null })), '驗 0.90／要 —')
})

test('bot 名：store 找得到用名字，找不到用 id 前 6 碼，沒有 bot 是 —', () => {
  const bots = [{ id: '01M1YDYX1247ZRWX8SY9MKNG2N', name: 'alfa' }]
  assert.equal(botLabel('01M1YDYX1247ZRWX8SY9MKNG2N', bots), 'alfa')
  assert.equal(botLabel('01GONEXXXXXXXXXXXXXXXXXXXX', bots), '01GONE')
  assert.equal(botLabel(null, bots), '—')
})

test('撞題：daemon 標成過時（matched_line 帶 stale）的列不算超過門檻，但總數照算', () => {
  const stale = row({ regex_verdict: 'same_work', at: ago(1), jev_same_work: 0.8, matched_line: '{"pair":"a|b","stale":"cancelled"}' })
  assert.equal(overThreshold(stale), false)
  const [, , same] = sceneStats([stale], NOW)
  assert.equal(same!.windows.all.total, 1)
  assert.equal(same!.windows.all.over, 0)
})

test('撞題：matched_line 沒有 stale、是 null、不是 JSON、stale 是空字串，照舊算超過門檻', () => {
  for (const matched_line of [null, '{"pair":"a|b"}', 'not json', '{"stale":""}']) {
    assert.equal(overThreshold(row({ regex_verdict: 'same_work', at: ago(1), jev_same_work: 0.8, matched_line })), true, String(matched_line))
  }
})

test('stale 只看撞題：別的場景 matched_line 恰好含 stale，門檻照舊', () => {
  assert.equal(overThreshold(row({ regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9, matched_line: '{"stale":"cancelled"}' })), true)
})

test('開票那條的 bot_id 是 issue:<號碼>：顯示成 #號碼（開票），不切成 issue:', () => {
  assert.equal(botLabel('issue:1330', []), '#1330（開票）')
  assert.equal(botLabel('issue:abc', []), 'issue:', '不是數字的照舊取前 6 碼')
})
