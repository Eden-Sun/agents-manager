import { test, afterEach } from 'node:test'
import assert from 'node:assert/strict'
import type { JudgeShadowRow } from '../api/judge'
import { DISMISSED_KEY, judgeBadgeFor, loadDismissed, popPosition, rememberDismissed } from './judgeBadge'

const NOW = Date.parse('2026-10-11T12:00:00Z')
const ago = (h: number) => new Date(NOW - h * 3600_000).toISOString()

function row(p: Partial<JudgeShadowRow> & Pick<JudgeShadowRow, 'id' | 'regex_verdict' | 'at'>): JudgeShadowRow {
  return {
    bot_id: 'bot-a',
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

const NONE = new Set<string>()

test('徽章：近 24 小時、該 bot、超過門檻的最新一筆；低於門檻、超過 24 小時、別的 bot 都不算', () => {
  const rows = [
    row({ id: 'low', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.3 }),
    row({ id: 'old', regex_verdict: 'stuck_queued', at: ago(30), jev_is_live_ui: 0.95 }),
    row({ id: 'other', regex_verdict: 'stuck_queued', at: ago(1), bot_id: 'bot-b', jev_is_live_ui: 0.95 }),
    row({ id: 'older-hit', regex_verdict: 'stuck_queued', at: ago(5), jev_is_live_ui: 0.8 }),
    row({ id: 'newest-hit', regex_verdict: 'same_work', at: ago(2), jev_same_work: 0.6 }),
  ]
  assert.equal(judgeBadgeFor(rows, 'bot-a', NOW, NONE)?.id, 'newest-hit')
  assert.equal(judgeBadgeFor(rows, 'bot-c', NOW, NONE), null)
})

test('撞限第二意見沒有門檻，不產生徽章', () => {
  const rows = [row({ id: 'limit', regex_verdict: 'limit_hit', at: ago(1), jev_is_live_ui: 0.99 })]
  assert.equal(judgeBadgeFor(rows, 'bot-a', NOW, NONE), null)
})

test('關掉的那一則不再算；關掉後會退到次新的超過門檻那一則', () => {
  const rows = [
    row({ id: 'a', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 }),
    row({ id: 'b', regex_verdict: 'stuck_queued', at: ago(2), jev_is_live_ui: 0.9 }),
  ]
  assert.equal(judgeBadgeFor(rows, 'bot-a', NOW, new Set(['a']))?.id, 'b')
  assert.equal(judgeBadgeFor(rows, 'bot-a', NOW, new Set(['a', 'b'])), null)
})

test('報告證據：claims <0.8 或 asks ≥0.8 才是超過門檻', () => {
  const rows = [row({ id: 'rep', regex_verdict: 'report_evidence', at: ago(1), claims_verified: 0.95, asks_parent_action: 0.85 })]
  assert.equal(judgeBadgeFor(rows, 'bot-a', NOW, NONE)?.id, 'rep')
  const quiet = [row({ id: 'quiet', regex_verdict: 'report_evidence', at: ago(1), claims_verified: 0.95, asks_parent_action: 0.2 })]
  assert.equal(judgeBadgeFor(quiet, 'bot-a', NOW, NONE), null)
})

// ── 關掉紀錄（localStorage）：每台瀏覽器各自；讀寫失敗都不能讓畫面壞掉。
const realLocal = (globalThis as { localStorage?: unknown }).localStorage
function fakeStorage(impl: { get?: () => string | null; set?: () => void }) {
  return {
    getItem: impl.get ?? (() => null),
    setItem: impl.set ?? (() => {}),
  }
}
afterEach(() => {
  if (realLocal === undefined) delete (globalThis as { localStorage?: unknown }).localStorage
  else (globalThis as { localStorage?: unknown }).localStorage = realLocal
})

test('關掉的紀錄存在 localStorage，重讀得回來；只留最近 200 則', () => {
  const store = new Map<string, string>()
  ;(globalThis as { localStorage?: unknown }).localStorage = {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, v),
  }
  let cur = new Set<string>()
  cur = rememberDismissed('x-1', cur)
  assert.deepEqual([...loadDismissed()], ['x-1'])
  for (let i = 0; i < 250; i++) cur = rememberDismissed(`n-${i}`, cur)
  assert.equal(cur.size, 200)
  assert.equal(JSON.parse(store.get(DISMISSED_KEY)!).length, 200)
})

test('localStorage 不能用（私密視窗、被擋）：讀回空、寫不進去也不丟錯', () => {
  ;(globalThis as { localStorage?: unknown }).localStorage = fakeStorage({
    get: () => {
      throw new Error('blocked')
    },
    set: () => {
      throw new Error('quota')
    },
  })
  assert.deepEqual([...loadDismissed()], [])
  assert.deepEqual([...rememberDismissed('y', NONE)], ['y'], '這次就在記憶體裡生效')
})

test('localStorage 裡的內容壞掉（不是 JSON、不是陣列）：當作沒有', () => {
  ;(globalThis as { localStorage?: unknown }).localStorage = fakeStorage({ get: () => '{not json' })
  assert.deepEqual([...loadDismissed()], [])
  ;(globalThis as { localStorage?: unknown }).localStorage = fakeStorage({ get: () => '{"a":1}' })
  assert.deepEqual([...loadDismissed()], [])
})

test('過時的撞題紀錄不產生徽章，次新的超過門檻那則接上', () => {
  const rows = [
    row({ id: 'stale-same', regex_verdict: 'same_work', at: ago(1), jev_same_work: 0.9, matched_line: '{"stale":"cancelled"}' }),
    row({ id: 'live-stuck', regex_verdict: 'stuck_queued', at: ago(2), jev_is_live_ui: 0.8 }),
  ]
  assert.equal(judgeBadgeFor(rows, 'bot-a', NOW, NONE)?.id, 'live-stuck')
  assert.equal(judgeBadgeFor(rows.slice(0, 1), 'bot-a', NOW, NONE), null)
})

// ── 浮層位置（#1381）：浮層是 fixed，位置由徽章鈕的矩形算，不被側欄的捲動區裁掉。

test('popPosition：放得下就貼著按鈕左緣、往下開', () => {
  assert.deepEqual(popPosition({ left: 87, top: 100, bottom: 116 }, { width: 280, height: 140 }, 1280, 800), { left: 87, top: 120 })
})

test('popPosition：右邊放不下就往左推到離右緣 8px', () => {
  assert.equal(popPosition({ left: 300, top: 100, bottom: 116 }, { width: 280, height: 140 }, 390, 800).left, 390 - 8 - 280)
})

test('popPosition：左邊最少留 8px', () => {
  assert.equal(popPosition({ left: 2, top: 100, bottom: 116 }, { width: 280, height: 140 }, 1280, 800).left, 8)
})

test('popPosition：下面放不下、上面放得下就往上翻', () => {
  assert.equal(popPosition({ left: 87, top: 700, bottom: 716 }, { width: 280, height: 140 }, 1280, 800).top, 700 - 4 - 140)
})

test('popPosition：上下都放不下就維持往下', () => {
  assert.equal(popPosition({ left: 87, top: 90, bottom: 106 }, { width: 280, height: 140 }, 1280, 200).top, 110)
})
