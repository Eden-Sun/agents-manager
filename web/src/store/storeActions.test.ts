/**
 * store 動作的不變量：樂觀更新一定要收得回來，找不到的那一筆不可以拖垮整個功能。
 * 這些都不是純函式測得到的——它們的 bug 長在「送失敗之後沒有人善後」那一段。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Mission, MissionDetail, Project } from '../api/types.ts'
import { queueFromComposer, settleComposerSend } from './queuedSend.ts'

const { useStore } = await import('./store.ts')

const bot = (id: string, extra: Partial<Bot> = {}) =>
  ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null, ...extra }) as Bot
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const json = (body: unknown, status: number) => new Response(JSON.stringify(body), { status })

function seed() {
  reset()
  useStore.setState({
    projects: [project()],
    bots: [bot('b1'), bot('b2')],
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    queuedSends: {},
    drafts: {},
    notices: [],
    missions: {},
    missionDetail: {},
    missionLoadErrors: {},
    missionsSupported: true,
    missionsCapped: {},
    selectedBotId: null,
    selectedProjectId: null,
  })
}

const settle = () => new Promise((r) => setTimeout(r, 10))
const noticeTexts = () => useStore.getState().notices.map((n) => n.text)

test('任務被清掉：收掉那一張卡，其他任務與「交給 AGM」不受影響', async () => {
  seed()
  const gone = { id: 'm1', project_id: 'p1' } as Mission
  useStore.setState({
    missions: { p1: [gone, { id: 'm2', project_id: 'p1' } as Mission] },
    missionDetail: { m1: { id: 'm1', project_id: 'p1' } as MissionDetail },
  })
  routeDaemon(() => json({ error: 'not_found', what: 'mission' }, 404))
  await useStore.getState().loadMission('m1')
  const s = useStore.getState()
  assert.equal(s.missionsSupported, true, '一筆找不到不等於這台 daemon 沒有群組任務')
  assert.equal(s.missionDetail.m1, undefined)
  assert.deepEqual(
    s.missions.p1.map((m) => m.id),
    ['m2'],
  )
  assert.match(s.missionLoadErrors.m1, /不在/)
})

test('重抓一筆已在清單裡的任務：原地換掉，不搬到最前面（畫面不在手指底下跳）', async () => {
  seed()
  const row = (id: string, text: string) => ({ id, project_id: 'p1', text, status: 'done' }) as Mission
  useStore.setState({ missions: { p1: [row('m1', 'a'), row('m2', 'b'), row('m3', 'c')] } })
  routeDaemon((r) =>
    r.path.includes('/missions/m3')
      ? json({ id: 'm3', project_id: 'p1', text: 'c（重抓）', status: 'done', events: [], assignments: [], revisions: [], parent: null }, 200)
      : json({}, 404),
  )
  await useStore.getState().loadMission('m3')
  assert.deepEqual(
    useStore.getState().missions.p1.map((m) => [m.id, m.text]),
    [
      ['m1', 'a'],
      ['m2', 'b'],
      ['m3', 'c（重抓）'],
    ],
  )
  // 清單裡沒有的（從連結點進來的舊任務）才插到最前面。
  routeDaemon(() => json({ id: 'm9', project_id: 'p1', text: '舊的', status: 'done', events: [], assignments: [], revisions: [], parent: null }, 200))
  await useStore.getState().loadMission('m9')
  assert.deepEqual(
    useStore.getState().missions.p1.map((m) => m.id),
    ['m9', 'm1', 'm2', 'm3'],
  )
})

test('任務卡按「暫停」：帶 reason 送出，daemon 收得下（不帶就是 422）', async () => {
  seed()
  useStore.setState({ missions: { p1: [{ id: 'm1', project_id: 'p1', status: 'open' } as Mission] } })
  // 照 daemon 的 `Json<PauseIn>`：`reason` 是必填字串，缺了在 handler 之前就被拒。
  routeDaemon((r) => {
    if (r.method === 'POST' && r.path.endsWith('/missions/m1/pause')) {
      const reason = (r.body as { reason?: unknown } | undefined)?.reason
      if (typeof reason !== 'string' || !reason) return json({ error: 'unprocessable', message: 'missing field `reason`' }, 422)
      return json({ id: 'm1', project_id: 'p1', status: 'paused', paused_reason: reason }, 200)
    }
    return json({ id: 'm1', project_id: 'p1', status: 'paused', paused_reason: 'user_pause', events: [], assignments: [], revisions: [], parent: null }, 200)
  })
  await useStore.getState().controlMission('m1', 'pause')
  assert.deepEqual(noticeTexts(), [], '不該跳「任務操作失敗」')
  const post = requests.find((r) => r.method === 'POST')
  assert.deepEqual(post?.body, { reason: 'user_pause' })
  assert.equal(useStore.getState().missions.p1[0].paused_reason, 'user_pause')
})

test('等你回答的舊任務不會被 50 筆已完成的新任務擠出清單；已結案那段標成「最近 N 筆」', async () => {
  seed()
  const at = (i: number) => `2026-09-${String(10 + Math.floor(i / 1000)).padStart(2, '0')}T00:00:${String(i % 60).padStart(2, '0')}Z`
  const waiting = { id: 'old-paused', project_id: 'p1', status: 'paused', paused_reason: 'max_rounds', created_at: '2026-09-01T00:00:00Z' }
  const doneRows = Array.from({ length: 50 }, (_, i) => ({ id: `d${i}`, project_id: 'p1', status: 'done', created_at: at(i + 1) }))
  // 照 daemon：各狀態分開篩、新的在前、`limit` 截斷。`all` 只回最新 50 筆，那筆等你回答的就不在裡面。
  routeDaemon((r) => {
    const q = new URL(r.path, 'http://x').searchParams
    const limit = Number(q.get('limit') ?? 100)
    const rows =
      q.get('status') === 'open' ? [waiting] : q.get('status') === 'done' ? doneRows : q.get('status') === 'cancelled' ? [] : [...doneRows, waiting]
    return json({ project_id: 'p1', missions: rows.slice(0, limit) }, 200)
  })
  await useStore.getState().loadMissions('p1')
  const s = useStore.getState()
  assert.ok(s.missions.p1.some((m) => m.id === 'old-paused'), '停著等回答的那筆要在清單裡，否則沒地方回答')
  assert.equal(s.missions.p1.filter((m) => m.status === 'done').length, 50)
  assert.deepEqual(s.missionsCapped.p1, { done: true, cancelled: false })
  assert.equal(s.missions.p1.at(-1)?.id, 'old-paused', '合起來仍是新的在前')
})

test('重連之後已載入的任務清單與卡片全部重抓（斷線期間的 mission_updated 收不到）', async () => {
  seed()
  useStore.setState({
    missions: { p1: [{ id: 'm1', project_id: 'p1', status: 'open' } as Mission] },
    missionDetail: { m1: { id: 'm1', project_id: 'p1' } as MissionDetail },
  })
  routeDaemon((r) =>
    r.path.includes('/missions/m1')
      ? json({ id: 'm1', project_id: 'p1', status: 'paused', paused_reason: 'max_rounds', events: [], assignments: [], revisions: [], parent: null }, 200)
      : json({ project_id: 'p1', missions: [] }, 200),
  )
  await useStore.getState().refreshLoadedMissions()
  assert.ok(
    requests.some((r) => r.path.includes('/projects/p1/missions')),
    '清單要重抓',
  )
  assert.ok(requests.some((r) => r.path.includes('/missions/m1')), '卡片的細節也要重抓')
  assert.equal(useStore.getState().missionDetail.m1?.paused_reason, 'max_rounds')
})

test('SPA fallback 的 404（body 不是 daemon 的錯誤）仍然當成這台沒有群組任務', async () => {
  seed()
  routeDaemon(() => new Response('<!doctype html>', { status: 404 }))
  await useStore.getState().loadMission('m9')
  assert.equal(useStore.getState().missionsSupported, false)
})

test('交給 AGM 的回應在路上斷掉：再按一次沿用同一個 crid，不會開出第二筆任務', async () => {
  seed()
  const input = { text: '幫我修 lint', delivery_mode: 'push_main', executor_kind: 'claude', on_5h_limit: 'wait' } as const
  routeDaemon(() => json({ error: 'upstream', message: 'connection dropped' }, 502))
  assert.equal(await useStore.getState().startMission('p1', { ...input }), null)
  // 使用者看到「交給 AGM 失敗」才再按一次，中間一定隔了一段時間：拿時間當 crid 的寫法在這裡才現形。
  await settle()
  routeDaemon(() => json({ mission: { id: 'm1', project_id: 'p1' }, created: false }, 200))
  assert.equal(await useStore.getState().startMission('p1', { ...input }), 'm1')
  const crids = requests
    .filter((r) => r.method === 'POST')
    .map((r) => (r.body as { client_request_id?: string }).client_request_id)
  assert.equal(crids.length, 2)
  assert.ok(crids[0], 'store 要自己給 crid')
  assert.equal(crids[0], crids[1], '重送必須是同一個 crid，daemon 才回同一筆')
})

test('交給 AGM 失敗後改了執行者再送：是另一個要求，不能沿用舊 crid 拿回舊的那筆', async () => {
  seed()
  const input = { text: '幫我修 型別', delivery_mode: 'push_main', executor_kind: 'claude', on_5h_limit: 'wait' } as const
  routeDaemon(() => json({ error: 'upstream', message: 'connection dropped' }, 502))
  assert.equal(await useStore.getState().startMission('p1', { ...input }), null)
  await useStore.getState().startMission('p1', { ...input, executor_kind: 'codex' })
  const crids = requests.filter((r) => r.method === 'POST').map((r) => (r.body as { client_request_id?: string }).client_request_id)
  assert.equal(crids.length, 2)
  assert.notEqual(crids[0], crids[1])
})

test('排序沒存起來：通知說回到原本的順序，畫面就真的要回去', async () => {
  seed()
  routeDaemon(() => json({ error: 'upstream', message: 'daemon rebuilding' }, 502))
  useStore.getState().moveBot('b2', 'b1')
  assert.deepEqual(useStore.getState().botOrder.p1, ['b2', 'b1'], '先樂觀套用')
  await settle()
  assert.equal(useStore.getState().botOrder.p1, undefined, '失敗後要讓位給 daemon 的順序')
  assert.ok(noticeTexts().some((t) => t.includes('回到原本的順序')))

  useStore.setState({ botOrder: { p1: ['b2', 'b1'] }, notices: [] })
  useStore.getState().moveBot('b1', 'b2')
  await settle()
  assert.deepEqual(useStore.getState().botOrder.p1, ['b2', 'b1'], '收回到這次拖動之前的那一份，不是清空')
})

test('專案排序沒存起來也一樣收回', async () => {
  seed()
  useStore.setState({ projects: [project(), { id: 'p2', label: 'p2', path: '/p2', host: 'local' } as Project] })
  routeDaemon(() => json({ error: 'upstream', message: 'daemon rebuilding' }, 502))
  useStore.getState().moveProject('p2', 'p1')
  assert.deepEqual(useStore.getState().projectOrder, ['p2', 'p1'])
  await settle()
  assert.deepEqual(useStore.getState().projectOrder, [], '失敗後回到原本的順序')
})

test('兩次排序重疊、第一次晚到失敗：不能蓋掉第二次成功的順序（#275）', async () => {
  seed()
  useStore.setState({ projects: [project(), { id: 'p2', label: 'p2', path: '/p2', host: 'local' } as Project, { id: 'p3', label: 'p3', path: '/p3', host: 'local' } as Project] })
  let n = 0
  routeDaemon(async () => {
    n += 1
    if (n === 1) {
      await new Promise((r) => setTimeout(r, 30))
      return json({ error: 'upstream', message: 'x' }, 502)
    }
    return json({ ok: true }, 200)
  })
  useStore.getState().moveProject('p3', 'p1')
  useStore.getState().moveProject('p2', 'p1')
  const want = useStore.getState().projectOrder
  await new Promise((r) => setTimeout(r, 60))
  assert.deepEqual(useStore.getState().projectOrder, want, '第二次已存成功，舊的失敗不能把順序退回去')
})

test('回合還在跑時排第二則：第一則退回輸入框，不是無聲消失', () => {
  seed()
  useStore.getState().queueSend('b1', '先跑一次測試', ['a1'])
  useStore.getState().queueSend('b1', '順便看一下 lint', [])
  const s = useStore.getState()
  assert.deepEqual(s.queuedSends.b1, { text: '順便看一下 lint', attachments: [] })
  assert.equal(s.drafts['bot:b1'], '先跑一次測試', '第一則要看得到，不能只留在記憶裡')
  assert.ok(noticeTexts().some((t) => t.includes('退回輸入框')))
})

/** 元件的 `setText` 就是寫 store 草稿；附件列在這裡用不到。 */
const composerIO = (botId: string) => ({
  setText: (v: string) => useStore.getState().setDraft(`bot:${botId}`, v),
  clearFiles: () => {},
  queueSend: useStore.getState().queueSend,
  restoreQueuedSend: useStore.getState().restoreQueuedSend,
})

test('照 ChatPanel 的呼叫順序排第二則：退回的第一則真的留在輸入框', () => {
  seed()
  const io = composerIO('b1')
  // 使用者打字 → Enter：草稿裡就是要排的那一則。
  useStore.getState().setDraft('bot:b1', '先跑一次測試')
  queueFromComposer(io, 'b1', '先跑一次測試', ['a1'])
  assert.equal(useStore.getState().drafts['bot:b1'], undefined, '排進去的那則不留在輸入框')
  useStore.getState().setDraft('bot:b1', '順便看一下 lint')
  queueFromComposer(io, 'b1', '順便看一下 lint', [])
  const s = useStore.getState()
  assert.deepEqual(s.queuedSends.b1, { text: '順便看一下 lint', attachments: [] })
  assert.equal(s.drafts['bot:b1'], '先跑一次測試', '通知說已退回輸入框，輸入框就要真的有')
  assert.ok(noticeTexts().some((t) => t.includes('退回輸入框') && t.includes('1 個附件')))
})

test('中止並取代送出的是排隊那則：輸入框裡被退回的上一則不能跟著清掉', () => {
  seed()
  useStore.setState({ queuedSends: { b1: { text: '第二則', attachments: [] } }, drafts: { 'bot:b1': '第一則' } })
  const wasQueued = useStore.getState().queuedSends.b1
  useStore.getState().cancelQueuedSend('b1')
  settleComposerSend(composerIO('b1'), 'b1', wasQueued, true)
  assert.equal(useStore.getState().drafts['bot:b1'], '第一則')

  // 沒排隊、送的是輸入框本身：照常清掉。
  settleComposerSend(composerIO('b1'), 'b1', null, true)
  assert.equal(useStore.getState().drafts['bot:b1'], undefined)
})

test('取消排隊：那則接回輸入框最前面，不蓋掉正在打的字', () => {
  seed()
  useStore.setState({ queuedSends: { b1: { text: '排隊那則', attachments: [] } }, drafts: { 'bot:b1': '打到一半' } })
  useStore.getState().unqueueToDraft('b1')
  const s = useStore.getState()
  assert.equal(s.queuedSends.b1, undefined)
  assert.equal(s.drafts['bot:b1'], '排隊那則\n打到一半')
})

test('已讀送不出去：daemon 的舊數字不可以把徽章點回來', async () => {
  seed()
  // 訊息載過了：否則 `loadMessages` 會用已讀標記重算，蓋掉這裡要看的那一步。
  useStore.setState({ botUnread: { b1: 3, b2: 1 }, loadedBots: { b1: true, b2: true } })
  routeDaemon((req) => {
    if (req.path.includes('/read')) return json({ error: 'upstream', message: 'daemon restarting' }, 502)
    if (req.path.startsWith('/api/state')) {
      return json(
        {
          daemon_seq: 1,
          connected: true,
          hosts: [],
          projects: [project()],
          bots: [{ ...bot('b1'), unread: 3 }, { ...bot('b2'), unread: 1 }],
          runs: [],
          turns: [],
          identities: [],
        },
        200,
      )
    }
    return json({}, 200)
  })
  useStore.getState().markBotRead('b1')
  await settle()
  assert.equal(useStore.getState().botUnread.b1, undefined, '本機先清掉')
  await useStore.getState().refreshState()
  const after = useStore.getState().botUnread
  assert.equal(after.b1, undefined, '送不出去的已讀還沒補上，快照的舊數字不算')
  assert.equal(after.b2, 1, '沒讀過的那顆照舊由 daemon 說了算')
})

const pane = (over: Record<string, unknown> = {}) => ({
  pane_id: 'w1:p9', host: 'local', workspace_id: 'w1', tab_id: 'w1:t1', cwd: '/p', kind: 'service', owned_by: 'user',
  owner_bot_id: null, project_id: 'p1', purpose: null, foreground: 'vim notes.md', listen_ports: [],
  last_output_at: '', first_seen: '', last_seen: '', gc_optin: false, ...over,
})

test('從選單點進 vim 的 shell（被分成 service、沒有 port）：要打得進去', () => {
  seed()
  useStore.getState().viewPane(pane() as never)
  assert.equal(useStore.getState().shellView?.readOnly, false, '照 kind 鎖的話人會卡在 vim 裡出不來')
  useStore.getState().viewPane(pane({ kind: 'shell', listen_ports: [3010] }) as never)
  assert.equal(useStore.getState().shellView?.readOnly, true, '有 port 的才唯讀')
  useStore.getState().viewPane(pane({ listen_ports: [3010], read_only: false }) as never)
  assert.equal(useStore.getState().shellView?.readOnly, false, 'daemon 給了 read_only 就照它')
})

test('從 / 重整回來：readOnly／traced 照 daemon 當下的 pane 列重算，不沿用 localStorage 的舊值', async () => {
  seed()
  useStore.setState({ shellView: { host: 'local', paneId: 'w1:p9', cwd: '/p', readOnly: true, traced: true } })
  routeDaemon((req) => {
    if (req.path.includes('/hosts/local/shells')) return json({ host: 'local', shells: [] }, 200)
    if (req.path.includes('/panes')) return json({ panes: [pane({ listen_ports: [] })] }, 200)
    return json({}, 200)
  })
  await useStore.getState().restoreShellView()
  assert.equal(useStore.getState().shellView?.readOnly, false)
  assert.equal(useStore.getState().shellView?.traced, true)
})

test('daemon 回 403：面板鎖成唯讀並帶著 daemon 的說明；別的 pane 不受影響', () => {
  seed()
  useStore.setState({ shellView: { host: 'local', paneId: 'w1:p9', cwd: '/p', readOnly: false, traced: true } })
  useStore.getState().lockShellView('local', 'w1:p8', '別顆')
  assert.equal(useStore.getState().shellView?.readOnly, false)
  useStore.getState().lockShellView('local', 'w1:p9', '這顆 pane 開著 port，只能看')
  assert.equal(useStore.getState().shellView?.readOnly, true)
  assert.equal(useStore.getState().shellView?.readOnlyReason, '這顆 pane 開著 port，只能看')
})

test('側欄與專案頁同一份 pane 清單：專案頁關掉的，側欄同時不見；正開著它的面板也收掉', async () => {
  seed()
  useStore.setState({
    sidePanes: { p1: [pane({ kind: 'shell' }) as never] },
    unownedPanes: [],
    shellView: { host: 'local', paneId: 'w1:p9', cwd: '/p', traced: true },
  })
  routeDaemon((req) => {
    if (req.path.includes('/close')) return json({ closed: true }, 200)
    if (req.path.includes('unowned=1')) return json({ panes: [] }, 200)
    if (req.path.includes('/panes')) return json({ panes: [] }, 200)
    return json({}, 200)
  })
  assert.equal(await useStore.getState().closeTracedPane(pane({ kind: 'shell' }) as never, false), 'closed')
  assert.equal(useStore.getState().sidePanes.p1, undefined)
  assert.equal(useStore.getState().shellView, null)
})

test('關閉時才發現已經變成服務 pane：拿 daemon 附上的那列回來問人，不是只跳 409', async () => {
  seed()
  useStore.setState({ sidePanes: { p1: [pane({ kind: 'shell' }) as never] }, unownedPanes: [] })
  const fresh = pane({ kind: 'service', listen_ports: [3010], foreground: 'node next dev' })
  routeDaemon(() => json({ error: 'conflict', reason: 'service_pane', pane: fresh }, 409))
  const r = await useStore.getState().closeTracedPane(pane({ kind: 'shell' }) as never, false)
  assert.ok(r && r !== 'closed')
  assert.equal(r.kind, 'service')
  assert.deepEqual(r.listen_ports, [3010])
  assert.equal(useStore.getState().sidePanes.p1[0].kind, 'service', '清單也換成最新那列')
  assert.equal(noticeTexts().length, 0, '這不是失敗')
})

test('點到已經不在的 pane：面板收掉、清單拿掉，而且講一聲', () => {
  seed()
  useStore.setState({
    sidePanes: { p1: [pane() as never] },
    unownedPanes: [],
    shellView: { host: 'local', paneId: 'w1:p9', cwd: '/p', traced: true },
  })
  routeDaemon(() => json({ panes: [] }, 200))
  useStore.getState().paneGone('local', 'w1:p9')
  assert.equal(useStore.getState().shellView, null)
  assert.equal(useStore.getState().sidePanes.p1, undefined)
  assert.ok(noticeTexts().some((t) => t.includes('已經關掉')))
})

test('重讀 pane：依 project_id 分組，沒歸屬的只進底部那一組', async () => {
  seed()
  const scratch = pane({ pane_id: 'w1:pS', project_id: null, owned_by: 'none', kind: 'shell' })
  routeDaemon((req) => {
    if (req.path.includes('unowned=1')) return json({ panes: [{ ...scratch, scratch: true }] }, 200)
    if (req.path.includes('/panes')) return json({ panes: [pane(), scratch] }, 200)
    return json({}, 200)
  })
  await useStore.getState().refreshPanes()
  const s = useStore.getState()
  assert.deepEqual(Object.keys(s.sidePanes), ['p1'])
  assert.deepEqual(s.unownedPanes.map((p) => [p.pane_id, p.scratch]), [['w1:pS', true]])
})

test('daemon 重啟後結束面板自己開的 shell：不在記憶體清單裡就走 pane 表關，不是回 200 什麼都沒關', async () => {
  seed()
  useStore.setState({ shellView: { host: 'local', paneId: 'w9:s1', cwd: '/p' } })
  routeDaemon((req) => {
    if (req.method === 'GET' && req.path.includes('/hosts/local/shells')) return json({ host: 'local', shells: [] }, 200)
    if (req.method === 'POST' && req.path.includes('/panes/w9%3As1/close')) return json({ closed: true }, 200)
    return json({}, 200)
  })
  await useStore.getState().endHostShell('local', 'w9:s1')
  const calls = requests.map((r) => `${r.method} ${r.path}`)
  assert.ok(calls.some((c) => c.startsWith('POST') && c.includes('/panes/w9%3As1/close') && !c.includes('confirm=true')), '沒看過 port 不帶 confirm：' + calls.join('\n'))
  assert.ok(!calls.some((c) => c.startsWith('DELETE')), '那支 DELETE 在重啟後什麼都不做')
  assert.equal(useStore.getState().shellView, null)
})

test('還在記憶體清單裡的照舊走 DELETE；pane 表關失敗時面板留著並跳通知', async () => {
  seed()
  useStore.setState({ shellView: { host: 'local', paneId: 'w9:s1', cwd: '/p' } })
  routeDaemon((req) => {
    if (req.method === 'GET') return json({ host: 'local', shells: [{ host: 'local', pane_id: 'w9:s1', cwd: '/p' }] }, 200)
    return json({}, 200)
  })
  await useStore.getState().endHostShell('local', 'w9:s1')
  assert.ok(requests.some((r) => r.method === 'DELETE'))
  assert.equal(useStore.getState().shellView, null)

  seed()
  useStore.setState({ shellView: { host: 'local', paneId: 'w9:s1', cwd: '/p' } })
  routeDaemon((req) => {
    if (req.method === 'GET') return json({ host: 'local', shells: [] }, 200)
    return json({ error: 'upstream', message: 'herdr unreachable' }, 502)
  })
  await useStore.getState().endHostShell('local', 'w9:s1')
  assert.notEqual(useStore.getState().shellView, null, '沒關掉就不能假裝關掉')
  assert.ok(noticeTexts().length > 0)
})

/** daemon 同時只准一批：第二次按回 `already_running`、total 0——不能當成「沒有要重啟的」蓋掉進度。 */
test('已經有一批在跑時再按一鍵重啟：不蓋掉進度，也不說沒有閒置的 bot', async () => {
  seed()
  const running = { id: 'batch-1', total: 3, done: 1, current: 'A', ok: ['Z'], failed: [], skipped: [], finished: false }
  useStore.setState({ restartBatch: running })
  routeDaemon((req) =>
    req.path.includes('/bots/restart-idle')
      ? json({ batch_id: 'batch-1', total: 0, planned: [], skipped: [], already_running: true }, 200)
      : json({}, 200),
  )
  await useStore.getState().restartIdleBots()
  assert.deepEqual(useStore.getState().restartBatch, running)
  assert.ok(!noticeTexts().some((t) => t.includes('沒有')), noticeTexts().join('|'))
  assert.ok(noticeTexts().some((t) => t.includes('已經有一批')))
})

/** AGM 驗收 9f05b03：服務 pane（在 listen）或讀不到狀態時，daemon 回 409 要人再確認——不能預設帶 confirm 把這道繞掉。 */
for (const [label, body, route] of [
  ['面板自己開的（DELETE）', { reason: 'service_pane', unverified: false, pane: { pane_id: 'w9:s1', host: 'local', kind: 'service', listen_ports: [3010] } }, 'own'],
  ['被 trace 的（pane 表）讀不到狀態', { reason: 'service_pane', unverified: true, pane: { pane_id: 'w9:s1', host: 'local', kind: 'shell', listen_ports: [] } }, 'traced'],
] as const) {
  test(`結束 shell 遇到要再確認（${label}）：不關、不跳錯誤，把 port／讀不到帶回去；確認後才帶 confirm`, async () => {
    seed()
    useStore.setState({ shellView: { host: 'local', paneId: 'w9:s1', cwd: '/p' } })
    const own = route === 'own' ? [{ host: 'local', pane_id: 'w9:s1', cwd: '/p' }] : []
    routeDaemon((req) => {
      if (req.method === 'GET' && req.path.includes('/hosts/local/shells')) return json({ host: 'local', shells: own }, 200)
      if (req.path.includes('confirm=true')) return json({ closed: true }, 200)
      return json({ error: 'conflict', ...body }, 409)
    })
    const needs = await useStore.getState().endHostShell('local', 'w9:s1')
    assert.ok(needs, '要回傳給面板再問一次')
    assert.equal(needs?.unverified, body.unverified)
    assert.deepEqual(needs?.pane?.listen_ports, body.pane.listen_ports)
    assert.notEqual(useStore.getState().shellView, null, '還沒確認就不能收掉面板')
    assert.equal(noticeTexts().length, 0, '要人確認不是錯誤')
    assert.ok(!requests.some((r) => r.path.includes('confirm=true')), '第一次不帶 confirm')

    const again = await useStore.getState().endHostShell('local', 'w9:s1', true)
    assert.equal(again, null)
    assert.ok(requests.some((r) => r.path.includes('confirm=true')), '確認後才帶 confirm')
    assert.equal(useStore.getState().shellView, null)
  })
}

/** GH #83：明確 identity 確認沒登入時啟動失敗，toast 要顯示 daemon 給的 `hint`（身分／主機／怎麼登入），不是原始 reason 代碼。 */
test('啟動撞到明確 identity 沒登入：toast 顯示 hint，不是 identity_not_logged_in 代碼', async () => {
  seed()
  routeDaemon(() =>
    json(
      {
        error: 'conflict',
        reason: 'identity_not_logged_in',
        identity: 'cc-lock',
        host: 'local',
        login_required: true,
        hint: '身份 `cc-lock` 在 local 沒有登入；先在該主機用 `cc-lock` 登入（例如 claude 的 `/login`），再重試啟動。',
      },
      409,
    ),
  )
  await useStore.getState().startBot('b1')
  const texts = noticeTexts()
  assert.equal(texts.length, 1)
  assert.match(texts[0], /cc-lock/)
  assert.match(texts[0], /登入/)
  assert.doesNotMatch(texts[0], /identity_not_logged_in/)
})

/** 其他 409（例如一般的 not_idle）沒有專屬文案時，照舊走 `errText` 的通用格式。 */
test('啟動撞到其他 409：沒有專屬文案時照舊顯示 reason（HTTP 碼）', async () => {
  seed()
  routeDaemon(() => json({ error: 'conflict', reason: 'not_idle', bot_id: 'b1', busy: 'working' }, 409))
  await useStore.getState().startBot('b1')
  assert.deepEqual(noticeTexts(), ['not_idle（HTTP 409）'])
})

const uncommittedRun = (error: string) =>
  json(
    {
      error,
      run_id: 'r1',
      retryable: true,
      message: 'agent 已經在跑，但 run 的狀態寫不進 DB；已排重試，會照 herdr 的狀態收斂成 running',
    },
    503,
  )

/** API.md：503 start_state_uncommitted＝agent 起來了、DB 還沒寫成。當失敗、不刷新的話側欄仍顯示停著，使用者再按一次就 409。 */
test('啟動 503 start_state_uncommitted：要刷新狀態，不能當成沒啟動', async () => {
  seed()
  routeDaemon((r) => (r.path.endsWith('/start') ? uncommittedRun('start_state_uncommitted') : json({}, 200)))
  await useStore.getState().startBot('b1')
  assert.ok(
    requests.some((r) => r.method === 'GET' && r.path.endsWith('/state')),
    `503 之後要 refreshState，實際請求：${requests.map((r) => r.method + ' ' + r.path).join(', ')}`,
  )
  assert.ok(noticeTexts().some((t) => /已經在跑/.test(t)))
})

/** 側欄還沒跟上時再按啟動：daemon 回 409 active run already exists——bot 其實在跑，要刷新而不是只報機器碼。 */
test('啟動 409 已有 active run：刷新狀態，不當成啟動失敗', async () => {
  seed()
  routeDaemon((r) =>
    r.path.endsWith('/start') ? json({ error: 'conflict', reason: 'active run already exists', run_id: 'r1' }, 409) : json({}, 200),
  )
  await useStore.getState().startBot('b1')
  assert.ok(requests.some((r) => r.method === 'GET' && r.path.endsWith('/state')))
  assert.equal(noticeTexts().some((t) => /active run already exists/.test(t)), false)
})

test('停止 503 stop_state_uncommitted：要刷新狀態，不能當成沒停', async () => {
  seed()
  routeDaemon((r) => (r.path.endsWith('/stop') ? uncommittedRun('stop_state_uncommitted') : json({}, 200)))
  await useStore.getState().stopBot('b1')
  assert.ok(requests.some((r) => r.method === 'GET' && r.path.endsWith('/state')))
})

test('重啟 503 restart_state_uncommitted：要刷新狀態', async () => {
  seed()
  routeDaemon((r) => (r.path.includes('/restart') ? uncommittedRun('restart_state_uncommitted') : json({}, 200)))
  assert.equal(await useStore.getState().restartBot('b1'), true)
  assert.ok(requests.some((r) => r.method === 'GET' && r.path.endsWith('/state')))
})

/** issue #112：刪 bot 成功只該跳一次帶「復原」的通知，不該在 refreshState 之後又補一則沒有復原的。 */
test('刪 Bot 成功：只跳一張帶「復原」的通知，不重複跳第二張', async () => {
  seed()
  routeDaemon(() => json({}, 200))
  await useStore.getState().removeBot('b1')
  const notices = useStore.getState().notices
  assert.equal(notices.length, 1, `應該只有一張通知，實際：${JSON.stringify(notices.map((n) => n.text))}`)
  assert.match(notices[0].text, /已刪除/)
  assert.ok(notices[0].action, '這張通知要帶「復原」')
})

/** 復原撞名 409：現況 `restoreBot` 吞掉錯誤，通知上按「復原」沒有任何回饋。 */
test('復原 Bot 失敗：要跳出錯誤，不能靜默什麼都不說', async () => {
  seed()
  routeDaemon(() => json({}, 200))
  await useStore.getState().removeBot('b1')
  const restore = useStore.getState().notices[0]?.action?.run
  assert.ok(restore)
  routeDaemon((r) =>
    r.path.includes('/restore')
      ? json({ error: 'conflict', reason: 'bot name already in use in this project', name: 'b1' }, 409)
      : json({}, 200),
  )
  await restore!()
  assert.match(noticeTexts().join('\n'), /復原失敗：bot name already in use in this project（HTTP 409）/)
})

/** issue #122：沒在跑的 bot 按送出——交給 daemon 先收下，瀏覽器不留一份、自己也不去按啟動。 */
test('沒在跑的 bot 送出：帶 start_if_stopped 交給 daemon，瀏覽器不排隊也不自己啟動', async () => {
  seed()
  useStore.setState({ runs: {}, turns: {}, messages: {} })
  routeDaemon(() => json({ turn_id: 't9', message_id: 'm9', delivery: 'queued' }, 200))
  const ok = await useStore.getState().sendPrompt('b1', '起來後幫我跑測試', [], false, true)
  assert.equal(ok, true)
  assert.equal(requests.length, 1, '只有一個請求：沒有另外 POST /start')
  assert.equal(requests[0].path.endsWith('/bots/b1/prompt'), true)
  assert.equal((requests[0].body as Record<string, unknown>).start_if_stopped, true)
  const s = useStore.getState()
  assert.equal(s.queuedSends.b1, undefined, '瀏覽器記憶體不是那一份')
  assert.equal(s.turns.b1.t9.status, 'queued')
  assert.equal(s.turns.b1.t9.awaitsStart, true)
})

test('取消等 bot 起來的那一則：daemon 撤回，文字接回輸入框最前面', async () => {
  seed()
  useStore.setState({
    runs: {},
    turns: { b1: { t9: { id: 't9', status: 'queued', awaitsStart: true, startError: null } as never } },
    messages: { b1: [{ id: 'm9', turn_id: 't9', role: 'user', content: '起來後幫我跑測試', attachments: [] } as never] },
    drafts: { 'bot:b1': '打到一半' },
  })
  routeDaemon((req) => (req.path.includes('/messages') ? json({ messages: [], turns: [], has_more: false }, 200) : json({}, 200)))
  await useStore.getState().cancelStartingSend('b1')
  assert.ok(requests.some((r) => r.method === 'POST' && r.path.endsWith('/turns/t9/withdraw')))
  assert.equal(useStore.getState().drafts['bot:b1'], '起來後幫我跑測試\n打到一半')
})

test('取消時已經送出去了（409）：不把文字塞回輸入框，講清楚撤不回來', async () => {
  seed()
  useStore.setState({
    runs: {},
    turns: { b1: { t9: { id: 't9', status: 'queued', awaitsStart: true, startError: null } as never } },
    messages: { b1: [{ id: 'm9', turn_id: 't9', role: 'user', content: '起來後幫我跑測試', attachments: [] } as never] },
    drafts: {},
  })
  routeDaemon((req) =>
    req.path.endsWith('/withdraw') ? json({ error: 'conflict', reason: 'turn is not waiting for its bot to start' }, 409) : json({ messages: [], turns: [], has_more: false }, 200),
  )
  await useStore.getState().cancelStartingSend('b1')
  assert.equal(useStore.getState().drafts['bot:b1'], undefined, '送出去的那則不會又出現在輸入框')
  assert.ok(noticeTexts().some((t) => t.includes('撤不回來')))
})

/**
 * #149／#147：字已經送進 bot，只是結果寫不進 DB——daemon 回 503，但 turn 與訊息都已經在對話裡（`message_added`／
 * `turn_updated` 先推了）。前端把它當「沒送出」：輸入框留著同一段字、排隊的被放回去，下一次 Enter／回合結束的 flush
 * 就用新的 client_request_id 再送一次——bot 收到兩則。API.md §5：**不是沒送，不要換新的 client_request_id 重送**。
 */
const uncommitted503 = (over: Record<string, unknown> = {}) =>
  json(
    {
      error: 'delivery_state_uncommitted',
      run_id: 'r1',
      turn_id: 't7',
      message_id: 'm7',
      delivery: 'ok',
      sent: true,
      retryable: true,
      message: '這一則的送達結果還沒寫進 DB（delivery 是看到的結果）；daemon 會自己補上。',
      ...over,
    },
    503,
  )

test('送達結果寫不進 DB（503 delivery_state_uncommitted、sent:true）：算送出去了，輸入框可以清，不能叫人重送', async () => {
  seed()
  useStore.setState({ runs: { b1: { id: 'r1', state: 'running', agent_status: 'idle' } as never }, turns: {}, messages: {} })
  routeDaemon(() => uncommitted503())
  const ok = await useStore.getState().sendPrompt('b1', '跑一下測試')
  assert.equal(ok, true, '字已經進了 bot；回 false 會讓輸入框留著同一段字、排隊的被放回去，之後又送一次')
  assert.equal(requests.length, 1)
  const t = useStore.getState().turns.b1?.t7
  assert.equal(t?.status, 'in_flight', '輸入框要像成功時一樣鎖上這一回合')
  const texts = noticeTexts()
  assert.equal(texts.length, 1)
  assert.match(texts[0], /已送出/)
  assert.match(texts[0], /不要重送/)
})

test('送達結果寫不進 DB、delivery:unknown（sent:null）：一樣不是沒送', async () => {
  seed()
  useStore.setState({ turns: {} })
  routeDaemon(() => uncommitted503({ delivery: 'unknown', sent: null }))
  assert.equal(await useStore.getState().sendPrompt('b1', 'x'), true)
})

test('寫不進 DB 但 herdr 明確拒收（delivery:failed、sent:false）：一個字都沒進去，照舊回 false 讓字留在輸入框', async () => {
  seed()
  useStore.setState({ turns: {} })
  routeDaemon(() => uncommitted503({ delivery: 'failed', sent: false }))
  assert.equal(await useStore.getState().sendPrompt('b1', 'x'), false)
  assert.equal(useStore.getState().turns.b1?.t7, undefined, '失敗的回合不能鎖輸入框')
})

test('插隊送出鍵已生效、狀態寫不進去（503 send_now_state_uncommitted、sent:true）：算送出去了', async () => {
  seed()
  routeDaemon(() =>
    json(
      {
        error: 'send_now_state_uncommitted',
        run_id: 'r1',
        turn_id: 't8',
        interrupted_turn_id: 't1',
        sent: true,
        retryable: true,
        message: '送出鍵已經生效了，但回合的狀態還沒寫成；daemon 會自己補上。',
      },
      503,
    ),
  )
  assert.equal(await useStore.getState().sendPrompt('b1', 'x', [], true), true)
  assert.match(noticeTexts()[0], /已送出/)
})

test('503 維護窗口讀不到（sent:false）跟別的 503 一樣：沒送出，回 false', async () => {
  seed()
  routeDaemon(() => json({ reason: 'maintenance_state_unavailable', retryable: true, sent: false, message: 'x' }, 503))
  assert.equal(await useStore.getState().sendPrompt('b1', 'x'), false)
})

/** 上面那個 503 落在「排隊那則被 flush」：以前 `flushQueued` 見 false 就把已經送進 bot 的那則放回佇列，下一個回合結束又送一次。 */
test('排隊的那則 flush 時撞到 503 delivery_state_uncommitted：不放回佇列（放回去＝下一輪再送一次）', async () => {
  seed()
  useStore.setState({ turns: {}, messages: {}, runs: {}, queuedSends: { b1: { text: '排隊那則', attachments: [] } } })
  routeDaemon((req) => {
    if (req.path.endsWith('/start')) return json({ run_id: 'r1' }, 200)
    if (req.path.endsWith('/state'))
      return json(
        {
          daemon_seq: 50,
          projects: [
            {
              id: 'p1',
              path: '/p',
              bots: [{ id: 'b1', name: 'b1', kind: 'claude', run: { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle' } }],
            },
          ],
        },
        200,
      )
    if (req.path.endsWith('/prompt')) return uncommitted503()
    return json({ messages: [], turns: [], has_more: false }, 200)
  })
  await useStore.getState().startBot('b1')
  await new Promise((r) => setTimeout(r, 500))
  assert.equal(requests.filter((r) => r.path.endsWith('/prompt')).length, 1, '有送出去一次')
  assert.equal(useStore.getState().queuedSends.b1, undefined, '已經送進 bot 的那則不能又躺回佇列')
})

/**
 * 插隊送出（#103、#120）的 200 有四個 `send_now`：interrupted／idle 是成功；`not_sent`（送出鍵沒生效，這一則 failed，
 * 字可能還留在終端輸入框）與 `unknown`（不知道生效沒有，這一則 failed＋delivery unknown，不佔 in-flight）是**沒插成**。
 * 以前前端一律說「沒有插隊：claude 還沒有 send-now 鍵，訊息照一般方式送出」——那兩種都沒有照一般方式送出。
 */
test('插隊送出鍵沒生效（send_now:not_sent）：說沒送出、字可能還在終端，不能說「照一般方式送出」', async () => {
  seed()
  useStore.setState({ turns: {}, messages: {} })
  routeDaemon((req) =>
    req.path.endsWith('/prompt')
      ? json({ turn_id: 't9', message_id: 'm9', delivery: 'failed', send_now: 'not_sent' }, 200)
      : json({ messages: [], turns: [], has_more: false }, 200),
  )
  const ok = await useStore.getState().sendPrompt('b1', '先看這句', [], true)
  assert.equal(ok, false, '沒送出：字留在輸入框')
  const texts = noticeTexts()
  assert.equal(texts.length, 1, `一則就夠，別再疊「delivery=failed」：${JSON.stringify(texts)}`)
  assert.doesNotMatch(texts[0], /照一般方式送出|還沒有 send-now/)
  assert.match(texts[0], /沒送出/)
  assert.match(texts[0], /終端/)
})

test('插隊送出不知道有沒有生效（send_now:unknown）：不叫人「放棄該回合」、不重送，也不把輸入框鎖進未知送達', async () => {
  seed()
  useStore.setState({ turns: {}, messages: {} })
  routeDaemon((req) =>
    req.path.endsWith('/prompt')
      ? json({ turn_id: 't9', message_id: 'm9', delivery: 'unknown', send_now: 'unknown' }, 200)
      : json({ messages: [], turns: [], has_more: false }, 200),
  )
  const ok = await useStore.getState().sendPrompt('b1', '先看這句', [], true)
  assert.equal(ok, true, '不知道有沒有進去：不留字在輸入框等人重送')
  const texts = noticeTexts()
  assert.equal(texts.length, 1, JSON.stringify(texts))
  assert.doesNotMatch(texts[0], /照一般方式送出|還沒有 send-now|放棄該回合/)
  assert.match(texts[0], /不重送|先別重送/)
  // 那一則已經是 failed、不佔 in-flight：沒有回合可放棄，本地不能記一筆「送達未知的進行中回合」把輸入框鎖住。
  assert.equal(useStore.getState().turns.b1?.t9, undefined)
})

test('插隊送出成功（interrupted／idle）不多說話', async () => {
  seed()
  useStore.setState({ turns: {}, messages: {} })
  routeDaemon(() => json({ turn_id: 't9', message_id: 'm9', delivery: 'ok', send_now: 'interrupted' }, 200))
  assert.equal(await useStore.getState().sendPrompt('b1', 'x', [], true), true)
  assert.deepEqual(noticeTexts(), [])
})

/**
 * 這幾天 daemon 新增的「暫時不行、之後可以原樣重來」的 409／503：`ApiError.message` 取 `reason`，
 * 通知就只剩 `composer_busy（HTTP 409）` 這種機器代碼——看不出是沒送出、字在不在、要等多久。
 */
test('送出撞 409 composer_busy／resume_unverified：講人話（沒送出、怎麼辦），不是機器代碼', async () => {
  seed()
  routeDaemon(() => json({ error: 'conflict', reason: 'composer_busy', run_id: 'r1', retryable: true, sent: false }, 409))
  assert.equal(await useStore.getState().sendPrompt('b1', 'x'), false)
  let texts = noticeTexts()
  assert.equal(texts.length, 1)
  assert.doesNotMatch(texts[0], /composer_busy/)
  assert.match(texts[0], /沒送出/)
  assert.match(texts[0], /輸入框/)

  seed()
  routeDaemon(() => json({ error: 'conflict', reason: 'resume_unverified', run_id: 'r1', session_id: 's1', retry_after_s: 90 }, 409))
  assert.equal(await useStore.getState().sendPrompt('b1', 'x'), false)
  texts = noticeTexts()
  assert.doesNotMatch(texts[0], /resume_unverified/)
  assert.match(texts[0], /沒送出/)
  assert.match(texts[0], /接回/)
})

test('daemon 帶了人話 message 的可重試錯誤（維護窗口、Esc 送出去了不知道進了沒有）：顯示 message，不顯示 reason 代碼', async () => {
  seed()
  routeDaemon(() =>
    json(
      { error: 'conflict', reason: 'maintenance_window', retryable: true, sent: false, message: 'am-agm 正在進行維護（restart 窗口），到 12:00 為止不送新的 prompt。' },
      409,
    ),
  )
  await useStore.getState().sendPrompt('b1', 'x')
  assert.match(noticeTexts()[0], /正在進行維護/)
  assert.doesNotMatch(noticeTexts()[0], /maintenance_window/)

  seed()
  routeDaemon(() =>
    json(
      { error: 'conflict', reason: 'interrupt_unconfirmed', esc_sent: 'unknown', retryable: true, message: 'Esc 送出去了但 herdr 沒有回，不知道進了沒有；回合先不收，等它的回聲或自己結束。' },
      409,
    ),
  )
  await useStore.getState().interruptBot('b1')
  assert.match(noticeTexts()[0], /不知道進了沒有/)
  assert.doesNotMatch(noticeTexts()[0], /interrupt_unconfirmed/)
})

/**
 * #233：daemon 的 409 帶了人話 `message`、但**沒有** `retryable`（登入指令找不到 CLI、default session 的 bot 不給開／關）時，
 * `reasonText` 只在 `retryable:true` 才用 `message`，通知就成了 `identity_login_unavailable（HTTP 409）` 這種機器代碼。
 */
test('身分登入 409 identity_login_unavailable：顯示 daemon 的人話 message，不是機器代碼', async () => {
  seed()
  routeDaemon(() =>
    json({ error: 'conflict', reason: 'identity_login_unavailable', host: 'm4p', identity: 'cc1', kind: 'claude', message: 'CLI 不在 PATH，無法登入' }, 409),
  )
  assert.equal(await useStore.getState().loginIdentity('m4p', 'cc1'), false)
  const texts = noticeTexts()
  assert.equal(texts.length, 1)
  assert.match(texts[0], /CLI 不在 PATH/)
  assert.doesNotMatch(texts[0], /identity_login_unavailable/)
})

test('啟動撞 409 default_session：顯示 daemon 的人話 message', async () => {
  seed()
  routeDaemon(() =>
    json(
      {
        error: 'conflict',
        reason: 'default_session',
        bot_id: 'b1',
        message: '這顆是從你自己的 herdr default session 匯入的，daemon 只觀察、不替它開或關 pane：要重啟請在那個終端裡自己做。',
      },
      409,
    ),
  )
  await useStore.getState().startBot('b1')
  const texts = noticeTexts()
  assert.match(texts[0], /default session/)
  assert.doesNotMatch(texts[0], /default_session/)
})

test('送出撞 409 host_unreadable（沒有 message）：講人話，不是機器代碼', async () => {
  seed()
  routeDaemon(() => json({ error: 'conflict', reason: 'host_unreadable', run_id: 'r1', retryable: true, sent: false }, 409))
  assert.equal(await useStore.getState().sendPrompt('b1', 'x'), false)
  const texts = noticeTexts()
  assert.doesNotMatch(texts[0], /host_unreadable/)
  assert.match(texts[0], /沒送出/)
  assert.match(texts[0], /主機/)
})

/**
 * 連點：第一下撤回成功之後，第二下拿到 409（那一則已經是 failed）。以前 `cancelStartingSend`／`abandonTurn` 沒有防連點，
 * 第二下的 409 被說成「撤不回來（可能已經送出）」／原始英文 409——而字明明已經接回輸入框。
 */
test('取消等 bot 起來的那一則連點兩下：只撤一次，不跳「撤不回來」', async () => {
  seed()
  useStore.setState({
    runs: {},
    turns: { b1: { t9: { id: 't9', status: 'queued', awaitsStart: true, startError: null } as never } },
    messages: { b1: [{ id: 'm9', turn_id: 't9', role: 'user', content: '起來後幫我跑測試', attachments: [] } as never] },
    drafts: {},
  })
  let withdrawn = 0
  routeDaemon((req) => {
    if (req.path.endsWith('/withdraw')) {
      withdrawn += 1
      // 第一下成功；那一則之後就是 failed，再撤回 409。回應慢一拍，第二下才來得及在第一下還沒回來時進場。
      return withdrawn === 1 ? json({}, 200) : json({ error: 'conflict', reason: 'turn is not waiting for its bot to start' }, 409)
    }
    return json({ messages: [], turns: [], has_more: false }, 200)
  })
  await Promise.all([useStore.getState().cancelStartingSend('b1'), useStore.getState().cancelStartingSend('b1')])
  assert.equal(withdrawn, 1, '第二下不該再送一次撤回')
  assert.equal(useStore.getState().drafts['bot:b1'], '起來後幫我跑測試', '字接回一次，不重複')
  assert.deepEqual(
    noticeTexts().filter((t) => t.includes('撤不回來')),
    [],
  )
})

test('放棄未知送達的回合連點兩下：只放棄一次', async () => {
  seed()
  let abandoned = 0
  routeDaemon((req) => {
    if (req.path.endsWith('/abandon')) {
      abandoned += 1
      return abandoned === 1 ? json({}, 200) : json({ error: 'conflict', reason: 'turn is neither in-flight nor of unknown delivery' }, 409)
    }
    return json({ messages: [], turns: [], has_more: false }, 200)
  })
  await Promise.all([useStore.getState().abandonTurn('b1', 't1'), useStore.getState().abandonTurn('b1', 't1')])
  assert.equal(abandoned, 1)
  assert.deepEqual(noticeTexts(), [])
})

test('快照在飛時 WS 已推進到更新的 seq：套完舊快照要再抓一次，不能讓舊的 run 狀態留著', async () => {
  seed()
  useStore.setState({ lastSeq: 1000, runs: {} })
  let stateFetches = 0
  const snapshot = (seq: number, agent: string) => ({
    daemon_seq: seq,
    projects: [
      {
        id: 'p1',
        path: '/p',
        label: 'p',
        bots: [{ id: 'b1', project_id: 'p1', name: 'b1', kind: 'claude', run: { id: 'r1', bot_id: 'b1', state: 'running', agent_status: agent } }],
      },
    ],
  })
  routeDaemon((req) => {
    if (req.path.endsWith('/state')) {
      stateFetches += 1
      if (stateFetches === 1) {
        // 快照是 seq 1000 時拍的；回應在路上時 `bot_status`（seq 1001）已經套用過。
        useStore.setState({ lastSeq: 1001 })
        return json(snapshot(1000, 'working'), 200)
      }
      return json(snapshot(1001, 'idle'), 200)
    }
    return json({ messages: [], turns: [], has_more: false }, 200)
  })
  await useStore.getState().refreshState()
  assert.equal(stateFetches, 2, '比快照新的 frame 已經到了，要補抓一次')
  assert.equal(useStore.getState().runs.b1?.agent_status, 'idle')
})

test('任務「暫停」連點兩下：只送一次，不跳「任務操作失敗」', async () => {
  seed()
  useStore.setState({ missions: { p1: [{ id: 'm1', project_id: 'p1', status: 'open' } as Mission] } })
  let pauses = 0
  routeDaemon((r) => {
    if (r.method === 'POST' && r.path.endsWith('/missions/m1/pause')) {
      pauses += 1
      return pauses === 1 ? json({ id: 'm1', project_id: 'p1', status: 'paused' }, 200) : json({ error: 'conflict', reason: 'already paused' }, 409)
    }
    return json({ id: 'm1', project_id: 'p1', status: 'paused', events: [], assignments: [], revisions: [], parent: null }, 200)
  })
  await Promise.all([useStore.getState().controlMission('m1', 'pause'), useStore.getState().controlMission('m1', 'pause')])
  assert.equal(pauses, 1)
  assert.deepEqual(noticeTexts(), [])
})

test('removeBot／addBot／addProject 連點：只送一次，不跳第二個失敗通知（#276）', async () => {
  seed()
  const hits = { del: 0, bot: 0, proj: 0 }
  routeDaemon((r) => {
    if (r.method === 'DELETE' && r.path.endsWith('/bots/b1')) {
      hits.del += 1
      return hits.del === 1 ? json({}, 200) : json({ error: 'not_found', what: 'bot' }, 404)
    }
    if (r.method === 'POST' && r.path.endsWith('/projects/p1/bots')) {
      hits.bot += 1
      return json({ bot_id: 'nb', name: 'x' }, 200)
    }
    if (r.method === 'POST' && r.path.endsWith('/projects')) {
      hits.proj += 1
      return hits.proj === 1 ? json({ project_id: 'np' }, 200) : json({ error: 'conflict', reason: 'project path already registered' }, 409)
    }
    return json({ daemon_seq: 1, connected: true, projects: [] }, 200)
  })
  const st = useStore.getState()
  await Promise.all([st.removeBot('b1'), st.removeBot('b1')])
  await Promise.all([st.addBot('p1', { name: 'x', kind: 'claude' } as never), st.addBot('p1', { name: 'x', kind: 'claude' } as never)])
  await Promise.all([st.addProject({ path: '/tmp/x' } as never), st.addProject({ path: '/tmp/x' } as never)])
  assert.deepEqual(hits, { del: 1, bot: 1, proj: 1 })
  assert.equal(noticeTexts().filter((t) => /not_found|already|找不到/.test(t)).length, 0)
})

test('多分頁：這個分頁存草稿不能洗掉別的分頁剛存的草稿', () => {
  seed()
  // 別的分頁在這個分頁載入之後才寫進 localStorage 的 bot9 草稿。
  localStorage.setItem('am.drafts', JSON.stringify({ 'bot:b9': '另一個分頁打的' }))
  useStore.getState().setDraft('bot:b1', '這個分頁打的')
  assert.deepEqual(JSON.parse(localStorage.getItem('am.drafts')!), { 'bot:b9': '另一個分頁打的', 'bot:b1': '這個分頁打的' })
  useStore.getState().setDraft('bot:b1', '')
  assert.deepEqual(JSON.parse(localStorage.getItem('am.drafts')!), { 'bot:b9': '另一個分頁打的' }, '清掉自己的鍵，別人的留著')
})

test('送出時連線斷了（回應遺失）：再送同一句要沿用同一個 client_request_id（#367）', async () => {
  seed()
  let n = 0
  routeDaemon(() => {
    n += 1
    if (n === 1) throw new TypeError('Failed to fetch')
    return json({ turn_id: 't1', message_id: 'm1', delivery: 'ok' }, 200)
  })
  assert.equal(await useStore.getState().sendPrompt('b1', '跑測試'), false)
  assert.equal(await useStore.getState().sendPrompt('b1', '跑測試'), true)
  const crids = requests.filter((r) => r.path.endsWith('/prompt')).map((r) => (r.body as { client_request_id?: string }).client_request_id)
  assert.equal(crids.length, 2)
  assert.equal(crids[0], crids[1])
  await useStore.getState().sendPrompt('b1', '跑測試')
  const last = requests.filter((r) => r.path.endsWith('/prompt')).at(-1)!.body as { client_request_id?: string }
  assert.notEqual(last.client_request_id, crids[0], '成功之後同一句是新的動作')
})

test('斷線重連後：已載入的對話要重抓訊息，不能只補狀態（#368）', async () => {
  seed()
  useStore.setState({ loadedBots: { b1: true } })
  routeDaemon(() => json({ messages: [], turns: [], has_more: false }, 200))
  const { reloadLoadedConversations } = await import('./store.ts')
  await reloadLoadedConversations(useStore.getState)
  assert.ok(requests.some((r) => r.path.includes('/bots/b1/messages')))
})

test('連不上 daemon／代理回空的 502：通知講人話，不是 Failed to fetch 或 HTTP 路徑（#370）', async () => {
  seed()
  routeDaemon(() => {
    throw new TypeError('Failed to fetch')
  })
  await useStore.getState().sendPrompt('b1', 'x')
  assert.ok(noticeTexts().some((t) => t.includes('連不上 daemon')), noticeTexts().join('|'))
  useStore.setState({ notices: [] })
  routeDaemon(() => new Response('Bad Gateway', { status: 502 }))
  await useStore.getState().sendPrompt('b1', 'y')
  assert.ok(noticeTexts().some((t) => t.includes('暫時不可用')), noticeTexts().join('|'))
})
