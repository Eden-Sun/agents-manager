import test from 'node:test'
import assert from 'node:assert/strict'
import { assignmentLabel, assignmentOpen, deliveryLabel, missionQna, missionView, pausedLabel, phaseLabel, quotaWaitText, MISSION_PHASES } from './missionView.ts'
import type { Mission, MissionEvent, MissionEventKind } from '../api/types.ts'

function mission(over: Partial<Mission> = {}): Mission {
  return {
    id: '01M1',
    project_id: '01P1',
    client_request_id: 'req-1',
    text: '把設定頁的錯字修掉',
    delivery_mode: 'pr',
    executor_kind: 'claude',
    on_5h_limit: 'wait',
    max_rounds: 2,
    rounds_used: 0,
    paused_reason: null,
    paused_detail: null,
    result_summary: null,
    parent_mission_id: null,
    status: 'open',
    phase: null,
    created_at: '2026-09-13T01:00:00Z',
    updated_at: '2026-09-13T01:00:00Z',
    completed_at: null,
    cancelled_at: null,
    ...over,
  }
}

let seq = 0
function ev(kind: MissionEventKind, text: string, payload: Record<string, unknown> | null = null): MissionEvent {
  seq += 1
  return {
    id: `e${String(seq).padStart(3, '0')}`,
    mission_id: '01M1',
    kind,
    text,
    relay_from: null,
    payload,
    reply_to: null,
    created_at: `2026-09-13T01:${String(seq).padStart(2, '0')}:00Z`,
  }
}

test('剛建立：五段停在「規劃」，沒有任何角色', () => {
  const v = missionView(mission(), [ev('instruction', '把設定頁的錯字修掉')])
  assert.equal(v.phase, 'planning')
  assert.equal(v.step, 0)
  assert.deepEqual(v.actors, [])
  assert.equal(v.ask, null)
})

test('角色從事件 payload 讀出來，照 執行者 → reviewer → 驗證者 排', () => {
  const v = missionView(mission(), [
    ev('report', '開工', { role: 'executor', bot: 'mission-exec', identity: 'cc2', model: 'opus' }),
    ev('report', 'approve', { role: 'reviewer', bot: 'mission-rev', identity: 'cc1' }),
    ev('verified', 'tsc/oxlint/build 都過', { role: 'verifier', bot: 'mission-ver', identity: 'cc0', model: 'fable' }),
  ])
  assert.deepEqual(
    v.actors.map((a) => [a.role, a.bot, a.identity, a.model]),
    [
      ['executor', 'mission-exec', 'cc2', 'opus'],
      ['reviewer', 'mission-rev', 'cc1', null],
      ['verifier', 'mission-ver', 'cc0', 'fable'],
    ],
  )
  // `verified` 之後就等交付
  assert.equal(v.phase, 'delivering')
  assert.equal(v.step, MISSION_PHASES.indexOf('delivering'))
  assert.equal(v.verified?.text, 'tsc/oxlint/build 都過')
})

test('進度只看「最遠走到哪」，回頭的事件不會把它拉回去', () => {
  const v = missionView(mission({ rounds_used: 1 }), [
    ev('report', '第一版', { role: 'executor', identity: 'cc2' }),
    ev('report', 'changes', { role: 'reviewer', identity: 'cc1' }),
    ev('round', 'reviewer 退回一次'),
    ev('report', '改好了', { role: 'executor', identity: 'cc2' }),
  ])
  assert.equal(v.phase, 'reviewing')
  assert.deepEqual(v.rounds, { used: 1, max: 2 })
})

/**
 * daemon 真的寫出來的換手 note（`supervisor/controller.rs::mission_quota`，鍵名一字不改）。
 * 以前測試用自編的 `{handoff, from, to}`，真 daemon 從來不寫，卡片接上去一律「? → ?」（review3 c1 L9）。
 */
function switchNote(assignmentId: string, from: string, to: string, model: string | null = null) {
  return ev('note', `${from} 撞到用量上限，換 ${to} 接手`, {
    mission_id: '01M1',
    assignment_id: assignmentId,
    role: 'executor',
    bot_id: 'bot-exec',
    from_identity: from,
    to_identity: to,
    model,
    reason: '週額度用完，換下一個身分',
    message: "You've hit your usage limit",
    needs_review: true,
  })
}

test('撞限換手讀 daemon 的 from_identity／to_identity，而且 note 不會被當成「誰在跑」', () => {
  const parent = asg({ id: 'a-first', target_bot_id: 'exec-a', status: 'awaiting_review', turn_status: 'identity_switch', turn_error: 'usage limit' })
  const child = asg({ follow_up_of: 'a-first', target_bot_id: 'exec-b' })
  const v = missionView(mission({ phase: 'executing' }), [switchNote('a-first', 'cc2', 'cc1', 'opus')], [parent, child])
  assert.deepEqual(
    v.handoffs.map((h) => [h.from, h.to, h.model, h.reason]),
    [['cc2', 'cc1', 'opus', '週額度用完，換下一個身分']],
    '同一次換手只算一行（交辦那邊推得出來的不再多一行「? → ?」）',
  )
  assert.equal(v.actors[0].bot, 'exec-b', '角色指向接手的那一顆，不是 daemon')
})

test('沒有第二個身分當 reviewer：讀 daemon 記的 decision，之後真的派出 reviewer 就撤掉標記', () => {
  const note = ev('note', '改走執行者自審＋驗證者把關', { decision: { decision: 'no_independent_reviewer', reason: '只有 cc2 有額度' } })
  assert.equal(missionView(mission(), [note], [asg({ role: 'executor' })]).soloReview, true)
  const later = asg({ role: 'reviewer', created_at: '2026-09-13T09:00:00Z' })
  assert.equal(missionView(mission(), [note], [asg({ role: 'executor' }), later]).soloReview, false)
})

test('停下來問人：原因、細節、問句、各身分的 Fable 重置時間都在卡片上', () => {
  const m = mission({
    paused_reason: 'no_fable_for_verifier',
    paused_detail: '三個身分的 Fable 週桶都見底了',
    status: 'paused',
  })
  const v = missionView(m, [
    ev('report', '改好了', { role: 'executor', identity: 'cc2' }),
    // `mission/api.rs::get_pick` 停下任務時記的形狀：resets 包在 decision 裡。
    ev('paused', '要等 Fable 額度回來，還是這次先不跑驗證？', {
      reason: 'no_fable_for_verifier',
      decision: {
        decision: 'ask_user',
        reason: '三個身分的 Fable 週桶都見底了',
        resets: [
          { identity: 'cc2', resets_at: '2026-09-14T00:00:00Z' },
          { identity: 'cc1', resets_at: '2026-09-15T00:00:00Z' },
        ],
      },
    }),
  ])
  assert.equal(v.phase, 'paused')
  // 停下來時進度停在停之前那一格，不是歸零
  assert.equal(v.step, MISSION_PHASES.indexOf('executing'))
  assert.equal(v.ask?.reason, 'no_fable_for_verifier')
  assert.equal(v.ask?.question, '要等 Fable 額度回來，還是這次先不跑驗證？')
  assert.deepEqual(v.ask?.resets.map((r) => r.identity), ['cc2', 'cc1'])
  assert.equal(pausedLabel('no_fable_for_verifier'), '沒有可用的 Fable 額度可以當驗證者')
})

test('交付與完成：delivered 的 sha／PR 網址讀得出來，完成就是最後一格', () => {
  const v = missionView(mission({ completed_at: '2026-09-13T02:00:00Z', status: 'done', result_summary: '錯字修好了' }), [
    ev('verified', '驗過了', { role: 'verifier', identity: 'cc0', model: 'fable' }),
    ev('delivered', '已開 PR', { mode: 'pr', branch: 'mission/01M1', url: 'https://github.com/x/y/pull/9' }),
    ev('completed', '錯字修好了'),
  ])
  assert.equal(v.phase, 'done')
  assert.equal(v.step, MISSION_PHASES.indexOf('done'))
  assert.equal(v.delivered?.url, 'https://github.com/x/y/pull/9')
  assert.equal(v.delivered?.branch, 'mission/01M1')
})

test('取消的任務不當成完成', () => {
  const v = missionView(mission({ cancelled_at: '2026-09-13T02:00:00Z', status: 'cancelled' }), [])
  assert.equal(v.phase, 'cancelled')
})

test('payload 讀不到就留白，不會壞掉也不會亂猜', () => {
  const v = missionView(mission(), [
    ev('report', '沒有 payload 的回報'),
    { ...ev('report', 'payload 是字串'), payload: null },
  ])
  assert.deepEqual(v.actors, [])
  assert.equal(v.phase, 'planning')
  assert.equal(v.latest?.text, 'payload 是字串')
})

test('沒認出來的暫停原因原樣顯示，不要吞掉', () => {
  assert.equal(pausedLabel('something_new'), 'something_new')
  assert.equal(pausedLabel('user_pause'), '你按了暫停')
  assert.equal(deliveryLabel('push_main'), '直接推 main')
  assert.equal(deliveryLabel('pr'), '開 PR')
})

// ---- P1b：daemon 給的 `phase` 與 `assignments[]` 才是權威（docs/API.md）

function asg(over: Partial<import('../api/types.ts').MissionAssignment> = {}) {
  seq += 1
  return {
    id: `a${seq}`,
    role: 'executor' as const,
    status: 'delivered',
    target_bot_id: 'bot-exec',
    turn_status: null,
    turn_error: null,
    follow_up_of: null,
    resume_at: null,
    created_at: `2026-09-13T02:${String(seq).padStart(2, '0')}:00Z`,
    completed_at: null,
    ...over,
  }
}

test('角色以交辦為準：bot 用 target_bot_id，身分／模型補事件裡的', () => {
  const v = missionView(
    mission({ phase: 'reviewing' }),
    [ev('report', '開工', { role: 'executor', bot: '舊的名字', identity: 'cc2', model: 'opus' })],
    [asg({ role: 'executor', target_bot_id: 'mission-exec' }), asg({ role: 'reviewer', target_bot_id: 'mission-rev' })],
  )
  assert.deepEqual(
    v.actors.map((a) => [a.role, a.bot, a.identity]),
    [
      ['executor', 'mission-exec', 'cc2'],
      ['reviewer', 'mission-rev', null],
    ],
  )
  assert.equal(v.phase, 'reviewing')
})

test('撞限換手：事件沒帶也認得出來（follow-up ＋ 上一件停在 identity_switch）', () => {
  const parent = asg({ id: 'a-parent', turn_status: 'identity_switch', turn_error: 'usage limit reached' })
  const child = asg({ follow_up_of: 'a-parent', target_bot_id: 'mission-exec-2' })
  const v = missionView(mission(), [], [parent, child])
  assert.equal(v.handoffs.length, 1)
  assert.equal(v.handoffs[0].reason, 'usage limit reached')
})

test('等額度／等 AGM 是 daemon 說的，卡片照實顯示且進度不倒退', () => {
  const waiting = missionView(mission({ phase: 'waiting_quota' }), [
    ev('report', '做到一半', { role: 'executor', identity: 'cc2' }),
  ])
  assert.equal(waiting.phase, 'waiting_quota')
  assert.equal(waiting.step, MISSION_PHASES.indexOf('executing'))

  const agm = missionView(mission({ phase: 'awaiting_agm' }), [
    ev('report', 'approve', { role: 'reviewer', identity: 'cc1' }),
  ])
  assert.equal(agm.phase, 'awaiting_agm')
  assert.equal(agm.step, MISSION_PHASES.indexOf('reviewing'))
  assert.equal(phaseLabel('awaiting_agm'), '等 AGM')
})

test('已結案的欄位仍然壓過 daemon 的 phase（cancelled / done / paused 先判）', () => {
  assert.equal(missionView(mission({ phase: 'executing', cancelled_at: 'x', status: 'cancelled' }), []).phase, 'cancelled')
  assert.equal(missionView(mission({ phase: 'executing', paused_reason: 'max_rounds', status: 'paused' }), []).phase, 'paused')
})

test('交辦的狀態講成人話：turn_status 比 status 精確，認不得的原樣顯示', () => {
  assert.equal(assignmentLabel(asg({ status: 'awaiting_review', turn_status: 'identity_switch' })), '撞限，等 AGM 換身分接手')
  assert.equal(assignmentLabel(asg({ status: 'awaiting_review', turn_status: 'quota_exhausted' })), '沒有可用額度')
  assert.equal(assignmentLabel(asg({ status: 'awaiting_review' })), '等 AGM 驗收')
  assert.equal(assignmentLabel(asg({ status: 'quota_blocked' })), '等額度重置')
  assert.equal(assignmentLabel(asg({ status: 'delivered' })), '進行中')
  assert.equal(assignmentLabel(asg({ status: 'something_new' })), 'something_new')
})

test('哪幾件交辦還開著：沒結案的才算，view 也把整串帶出去', () => {
  const open = asg({ status: 'delivered', completed_at: null })
  const closed = asg({ status: 'completed', completed_at: '2026-09-13T03:00:00Z' })
  const dropped = asg({ status: 'superseded', completed_at: null })
  assert.equal(assignmentOpen(open), true)
  assert.equal(assignmentOpen(closed), false)
  assert.equal(assignmentOpen(dropped), false)
  const v = missionView(mission(), [], [closed, open])
  assert.deepEqual(v.assignments.map((a) => a.id), [closed.id, open.id])
})

test('最新進度不含 instruction：卡片標題那一句不會再印第二次（P4 缺陷 4）', () => {
  const m = mission({ text: '把設定頁的錯字修掉' })
  const only = missionView(m, [ev('instruction', '把設定頁的錯字修掉')])
  assert.equal(only.latest, null)
  // 事件文字剛好等於任務本文的（AGM 原樣轉述）也算重複，一樣不當成進度
  const echoed = missionView(m, [ev('instruction', m.text), ev('report', m.text)])
  assert.equal(echoed.latest, null)
  const real = missionView(m, [ev('instruction', m.text), ev('report', '改好 4 處')])
  assert.equal(real.latest?.text, '改好 4 處')
})

test('追問與回覆靠 reply_to 配對，不是靠時間順序猜', () => {
  const q1 = ev('question', '這會影響登入嗎？')
  const q2 = ev('question', '那設定頁呢？')
  // 第二個問題先被回答：光看順序會配錯，reply_to 才是對的依據。
  const a2: MissionEvent = { ...ev('answer', '設定頁沒動到'), reply_to: q2.id }
  const a1: MissionEvent = { ...ev('answer', '不會，只動文案'), reply_to: q1.id }
  const pairs = missionQna([q1, q2, a2, a1])
  assert.equal(pairs.length, 2)
  assert.equal(pairs[0]?.question.id, q1.id)
  assert.equal(pairs[0]?.answer?.text, '不會，只動文案')
  assert.equal(pairs[1]?.answer?.text, '設定頁沒動到')
})

test('還沒被回答的追問留 null，畫面才說得出「還在等」', () => {
  const q = ev('question', '有副作用嗎？')
  const pairs = missionQna([q])
  assert.equal(pairs.length, 1)
  assert.equal(pairs[0]?.answer, null)
})

test('沒有 reply_to 的 answer 不會被硬配給某個追問', () => {
  // 使用者回答「停下來問人」也是 answer，但它不是在回哪一則追問。
  const q = ev('question', '要不要順便改標題？')
  const loose = ev('answer', '照你說的做')
  const pairs = missionQna([q, loose])
  assert.equal(pairs[0]?.answer, null, '不相干的回答不能被當成這題的答案')
})

test('沒有追問時就沒有問答串', () => {
  assert.deepEqual(missionQna([ev('report', '做完了'), ev('completed', 'ok')]), [])
})

test('執行者驗收後輪到 AGM：daemon 的事件不帶 role，進度點仍停在「執行」，不退回「規劃」', () => {
  // 真 daemon 的事件：instruction、AGM 用 CLI 回報（沒有 payload）。角色只在交辦上。
  const v = missionView(
    mission({ phase: 'awaiting_agm' }),
    [ev('instruction', '把設定頁的錯字修掉'), ev('report', '執行者做完了，派 reviewer 中')],
    [asg({ role: 'executor', status: 'completed', completed_at: '2026-09-13T03:00:00Z' })],
  )
  assert.equal(v.phase, 'awaiting_agm')
  assert.equal(v.step, MISSION_PHASES.indexOf('executing'))
})

test('等額度說明照任務的 5h 撞限選項講，看得到預計重送時間', () => {
  const blocked = asg({ status: 'quota_blocked', resume_at: '2026-09-13T08:00:00Z' })
  const fmt = (iso: string) => `<${iso}>`
  const wait = quotaWaitText({ on_5h_limit: 'wait', executor_kind: 'claude' }, blocked, fmt)
  assert.match(wait, /等重置/)
  assert.match(wait, /<2026-09-13T08:00:00Z>/)
  const sw = quotaWaitText({ on_5h_limit: 'switch', executor_kind: 'claude' }, blocked, fmt)
  assert.doesNotMatch(sw, /選的「等重置」/, '選了換身分的任務不能說是自己選了等')
  assert.match(sw, /身分額度都用完/)
  assert.match(quotaWaitText({ on_5h_limit: 'switch', executor_kind: 'codex' }, null), /codex 只有一把額度/)
})
