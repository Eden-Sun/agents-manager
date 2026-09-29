import test from 'node:test'
import assert from 'node:assert/strict'
import { cliUpdateDone, cliUpdateDoneMany, cliUpdateProgress, cliUpdateProgressMany, reconcileCliUpdate, reconcileCliUpdates, type CliUpdate } from './cliUpdate.ts'

const base = { update_id: 'u1', host: 'local', kind: 'codex' }

test('進度照階段走，版本沿用前一則', () => {
  let cur: CliUpdate | null = { id: 'u1', host: 'local', kind: 'codex', phase: 'starting', from: null, to: null }
  cur = cliUpdateProgress(cur, { ...base, phase: 'installing', from: '0.155.1' })
  assert.equal(cur?.phase, 'installing')
  cur = cliUpdateProgress(cur, { ...base, phase: 'verifying' })
  assert.deepEqual([cur?.phase, cur?.from], ['verifying', '0.155.1'])
  // 看不懂的階段不動。
  assert.equal(cliUpdateProgress(cur, { ...base, phase: '???' }), cur)
})

test('多台 Claude 安裝各自保留進度，done 只移除對應主機', () => {
  let cur = cliUpdateProgressMany([], { update_id: 'u-local', host: 'local', kind: 'claude', phase: 'installing', from: '2.1.281' })
  cur = cliUpdateProgressMany(cur, { update_id: 'u-m4p', host: 'm4p', kind: 'claude', phase: 'checking' })
  assert.deepEqual(cur.map((item) => [item.id, item.host, item.phase]), [['u-local', 'local', 'installing'], ['u-m4p', 'm4p', 'checking']])
  assert.deepEqual(cliUpdateDoneMany(cur, { update_id: 'u-local', kind: 'claude', ok: true, restart_required: true }), [cur[1]])
  assert.deepEqual(reconcileCliUpdates(cur, [{ update_id: 'u-m4p', host: 'm4p' }]), [cur[1]])
})

test('Claude 安裝完成只提示另行確認重啟', () => {
  const r = cliUpdateDone({ update_id: 'u-claude', host: 'm4p', kind: 'claude', ok: true, from: '2.1.281', to: '2.1.284', restart: null, restart_required: true })
  assert.equal(r.batch, null)
  assert.equal(r.joinBatchId, null)
  assert.match(r.message, /m4p 的 claude 已升到 2\.1\.281 → 2\.1\.284/)
  assert.match(r.message, /確認後再重啟/)
})

test('別的分頁按的也收；另一個 id 的舊事件不蓋掉手上這一個', () => {
  const adopted = cliUpdateProgress(null, { ...base, phase: 'checking' })
  assert.equal(adopted?.id, 'u1')
  const mine: CliUpdate = { id: 'u2', host: 'local', kind: 'codex', phase: 'installing', from: null, to: null }
  assert.equal(cliUpdateProgress(mine, { ...base, phase: 'verifying' }), mine)
})

test('失敗：不開批次，訊息講原因與錯誤', () => {
  const r = cliUpdateDone({ ...base, ok: false, reason: 'install_failed', error: 'curl: (6) Could not resolve host' })
  assert.equal(r.ok, false)
  assert.equal(r.batch, null)
  assert.match(r.message, /安裝失敗/)
  assert.match(r.message, /沒有重啟任何 Bot/)
  assert.match(r.message, /Could not resolve host/)
  const same = cliUpdateDone({ ...base, ok: false, reason: 'version_unchanged', from: '0.155.1', to: '0.155.1' })
  assert.match(same.message, /版本沒變/)
  assert.equal(same.batch, null)
  const below = cliUpdateDone({ ...base, ok: false, reason: 'target_not_reached', from: '0.155.0', to: '0.156.0', target_version: '0.157.0' })
  assert.match(below.message, /還沒到確認的版本/)
  assert.match(below.message, /沒有重啟任何 Bot/)
  assert.equal(below.batch, null)
})

test('#564：主機端的鎖被別的安裝拿著／重啟接手：訊息講清楚，不開批次', () => {
  const locked = cliUpdateDone({ ...base, ok: false, reason: 'already_running', error: 'local 已經有另一個 codex 安裝在跑' })
  assert.match(locked.message, /另一個安裝在跑/)
  assert.equal(locked.batch, null)
  const cut = cliUpdateDone({ ...base, ok: false, reason: 'interrupted', recovered: true })
  assert.match(cut.message, /daemon 在安裝途中重啟過/)
  assert.match(cut.message, /沒有重啟任何 Bot/)
  const ok = cliUpdateDone({ ...base, ok: true, recovered: true, to: '0.157.0', restart: null, restart_error: 'daemon 在安裝途中重啟過，這次沒有自動重啟 bot' })
  assert.equal(ok.batch, null)
  assert.match(ok.message, /沒有自動重啟/)
})

test('成功：一鍵重啟的計畫變成 header 的進度', () => {
  const r = cliUpdateDone({
    ...base,
    ok: true,
    from: '0.155.1',
    to: '0.157.0',
    restart: { batch_id: 'b1', total: 2, planned: [], skipped: [{ bot_id: 'x', name: 'cx-busy', reason: 'working', reason_label: '正在跑' }] },
  })
  assert.equal(r.ok, true)
  assert.equal(r.batch?.id, 'b1')
  assert.equal(r.batch?.total, 2)
  assert.equal(r.batch?.finished, false)
  assert.equal(r.batch?.skipped[0]?.name, 'cx-busy')
  assert.match(r.message, /0\.155\.1 → 0\.157\.0/)
})

test('成功但已經有一批在跑：接那一批，訊息請人等那批跑完再按', () => {
  const r = cliUpdateDone({ ...base, ok: true, to: '0.157.0', restart: { batch_id: 'b0', total: 0, planned: [], skipped: [], already_running: true } })
  assert.equal(r.batch, null)
  assert.equal(r.joinBatchId, 'b0')
  const noPlan = cliUpdateDone({ ...base, ok: true, to: '0.157.0', restart: null, restart_error: 'db locked' })
  assert.equal(noPlan.batch, null)
  assert.match(noPlan.message, /db locked/)
})

test('#566 排在不相干的那一批後面（deferred）：不接那一批的進度，講清楚跑完會自己接著重啟、不用再按', () => {
  const r = cliUpdateDone({
    ...base,
    ok: true,
    to: '0.157.0',
    restart_status: 'deferred',
    restart: { batch_id: null, total: 0, planned: [], skipped: [], restart_status: 'deferred', behind_batch_id: 'b0' },
  })
  assert.equal(r.ok, true)
  assert.equal(r.batch, null)
  assert.equal(r.joinBatchId, null, '不能把 codex 的升級掛到不相干的那一批')
  assert.match(r.message, /自動/)
  assert.doesNotMatch(r.message, /再按一次/)
})

test('#566 正在跑的那一批確實排著這台的 codex（already_covered）：接那一批的進度', () => {
  const r = cliUpdateDone({
    ...base,
    ok: true,
    to: '0.157.0',
    restart_status: 'already_covered',
    restart: { batch_id: 'b0', total: 0, planned: [], skipped: [], already_running: true, restart_status: 'already_covered' },
  })
  assert.equal(r.joinBatchId, 'b0')
  assert.doesNotMatch(r.message, /再按一次/)
})

test('快照對帳：daemon 說沒在裝就清掉；不知道（舊 daemon）不動；剛按下還沒拿到 id 的不動', () => {
  const cur: CliUpdate = { id: 'u1', host: 'local', kind: 'codex', phase: 'installing', from: null, to: null }
  assert.equal(reconcileCliUpdate(cur, [{ update_id: 'u1', host: 'local' }]), cur)
  assert.equal(reconcileCliUpdate(cur, []), null)
  assert.equal(reconcileCliUpdate(cur, undefined), cur)
  const starting: CliUpdate = { ...cur, id: '', phase: 'starting' }
  assert.equal(reconcileCliUpdate(starting, []), starting)
})

test('磁碟本來就是目標版本（沒跑安裝指令）：講「本來就是」，照樣接手重啟的進度', () => {
  const r = cliUpdateDone({
    ...base,
    ok: true,
    already_installed: true,
    from: '0.157.0',
    to: '0.157.0',
    restart: { batch_id: 'b2', total: 1, planned: [], skipped: [] },
  })
  assert.equal(r.batch?.id, 'b2')
  assert.match(r.message, /本來就是 0\.157\.0/)
  assert.doesNotMatch(r.message, /→/)
})
