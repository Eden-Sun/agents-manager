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
