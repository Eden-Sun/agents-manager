import test from 'node:test'
import assert from 'node:assert/strict'
import { anyAnalysed, cmpVersion, parseTriageRows, triageForRange } from './releaseTriage.ts'
import { updateRange } from './updateRange.ts'

/** 2026-09-25 真 daemon `GET /api/release-triage?kind=codex` 的形狀（節錄）：`verdicts` 是交回來的整份。 */
const RAW = {
  publish_enabled: false,
  repo: null,
  rows: [
    {
      kind: 'codex', version: '0.157.0', status: 'judged',
      entries: [
        { id: 'a1', text: 'Enabled fullscreen transcripts by default.' },
        { id: 'a2', text: 'Markdown lists now render with •.' },
        { id: 'a3', text: 'Added GPT-6 Sol.' },
        { id: 'a4', text: 'exec --json adds usage.' },
      ],
      verdicts: {
        verdicts: [
          { entry_id: 'a1', verdict: 'guard', reason: '畫面判讀會失效', module: 'lifecycle/start.rs' },
          { entry_id: 'a2', verdict: 'guard', reason: '回覆會被截斷', module: 'lifecycle/screen.rs' },
          { entry_id: 'a3', verdict: 'none', reason: '', module: '' },
          { entry_id: 'a4', verdict: 'upgrade-arg', reason: '用量可以直接讀', module: 'quota.rs' },
        ],
        issues: [
          { entry_ids: ['a1'], title: '全螢幕 transcript 預設開（提防）', triage: 'guard', duplicate_of: 549 },
          { entry_ids: ['a2'], title: '清單改用 •，回覆被截斷（提防）', triage: 'guard' },
        ],
      },
      issues: [],
    },
    { kind: 'codex', version: '0.156.1', status: 'empty', entries: [], verdicts: null, issues: [] },
    { kind: 'codex', version: '0.156.0', status: 'dispatched', entries: [], verdicts: null, issues: [] },
    { kind: 'codex', version: '0.155.1', status: 'judged', entries: [], verdicts: { verdicts: [], issues: [] }, issues: [] },
  ],
}

test('區間是 (from, to]：跑著的那一版不算，新版含；新的在前', () => {
  const { rows, repo } = parseTriageRows(RAW)
  const vs = triageForRange(rows, repo, '0.155.1', '0.157.0')
  assert.deepEqual(vs.map((v) => v.version), ['0.157.0', '0.156.1', '0.156.0'])
  assert.deepEqual(vs.map((v) => v.state), ['judged', 'empty', 'pending'])
})

test('from 不明：只看 to 那一版（跟 daemon 的 pick_sections 同一條規則）', () => {
  const { rows } = parseTriageRows(RAW)
  assert.deepEqual(triageForRange(rows, null, null, '0.157.0').map((v) => v.version), ['0.157.0'])
})

test('帳本沒有 to 那一版：補一列「尚未分析」，整個區間算沒分析過', () => {
  const { rows } = parseTriageRows(RAW)
  const vs = triageForRange(rows, null, '0.157.0', '0.158.0')
  assert.deepEqual(vs.map((v) => [v.version, v.state]), [['0.158.0', 'missing']])
  assert.equal(anyAnalysed(vs), false)
  assert.equal(anyAnalysed(triageForRange(rows, null, '0.155.1', '0.157.0')), true)
})

test('提案帶標題與連結；已寫成提案的條目不再逐條重複，值得早升另列', () => {
  const { rows } = parseTriageRows(RAW)
  const [v] = triageForRange(rows, 'Eden-Sun/agents-manager', null, '0.157.0')
  assert.ok(v)
  assert.deepEqual(v.issues.map((i) => [i.title, i.verdict, i.number, i.duplicate]), [
    ['全螢幕 transcript 預設開', 'guard', 549, true],
    ['清單改用 •，回覆被截斷', 'guard', null, false],
  ])
  assert.equal(v.issues[0]?.url, 'https://github.com/Eden-Sun/agents-manager/issues/549')
  assert.deepEqual(v.guard, [], '兩條提防都已經寫成提案')
  assert.deepEqual(v.upgrade.map((i) => i.reason), ['用量可以直接讀'])
})

test('沒設 repo：只有已開 issue 自己的 url 連得出去，duplicate_of 只顯示編號', () => {
  const raw = structuredClone(RAW)
  raw.rows[0]!.issues = [{ marker: 'm', entry_ids: ['a2'], number: 600, url: 'https://example/600' }] as never
  const { rows, repo } = parseTriageRows(raw)
  const [v] = triageForRange(rows, repo, null, '0.157.0')
  assert.equal(v?.issues[0]?.url, null)
  assert.equal(v?.issues[1]?.url, 'https://example/600')
  assert.equal(v?.issues[1]?.number, 600)
})

test('版本數值比較：0.9.0 < 0.10.0', () => {
  assert.ok(cmpVersion('0.9.0', '0.10.0') < 0)
  assert.ok(cmpVersion('2.1.10', '2.1.9') > 0)
  assert.equal(cmpVersion('1.0', '1.0.0'), 0)
})

test('codex 的版本區間從通知讀（三種文案），claude 用跑著的版本、新版交給 daemon 讀磁碟', () => {
  assert.deepEqual(updateRange('codex', 'codex 有新版 0.155.1 → 0.157.0，需安裝後重啟', null), { from: '0.155.1', to: '0.157.0' })
  assert.deepEqual(updateRange('codex', 'codex 有新版 0.157.0（這個 run 跑的是 0.155.1），已安裝，重啟套用', null), { from: '0.155.1', to: '0.157.0' })
  assert.deepEqual(updateRange('codex', 'codex 有新版 0.157.0，需安裝後重啟', null), { from: null, to: '0.157.0' })
  assert.deepEqual(updateRange('claude', 'Update installed · Restart to update', '2.1.280'), { from: '2.1.280', to: null })
  assert.deepEqual(updateRange('claude', '磁碟上已是 2.1.282（這個 run 跑的是 2.1.280）· 重啟套用', '2.1.280'), { from: '2.1.280', to: null })
})
