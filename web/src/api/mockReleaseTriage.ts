/**
 * mock 的更新框資料（issue #561）：`/changelog`、`/release-triage`、`/claude-update/review`。
 * 截圖要演「有分析／尚未分析」兩種：`__amMock.triageOff('codex')` 清掉某個 kind 的帳本、
 * `__amMock.updateNotice('am-codex', '…')` 給某顆 bot 掛更新通知。
 */

type Rec = Record<string, unknown>

/** 「磁碟上」的版本：claude 的新版已經下載好；codex 是舊版（新版還沒裝）。 */
const DISK: Record<string, string> = { claude: '2.1.282', codex: '0.155.1' }

const CHANGELOG: Record<string, { version: string; body: string }[]> = {
  claude: [
    { version: '2.1.282', body: '- Added `--resume-ask` to re-open the last question after resume\n- Fixed statusLine `version` missing after /login' },
    { version: '2.1.281', body: '- Changed Stop hook payload: `last_assistant_message` is now truncated at 64k' },
  ],
  codex: [
    { version: '0.157.0', body: '- Enabled fullscreen transcripts by default\n- Markdown lists render with •\n- New `f` shortcut forks the conversation' },
    { version: '0.156.1', body: '- Fixed resume picker ordering' },
    { version: '0.156.0', body: '- Background app-server starts automatically\n- `codex exec --json` adds `turn.usage`' },
  ],
}

function triageRows(kind: string): Rec[] {
  if (kind === 'claude') {
    return [
      {
        kind, version: '2.1.282', status: 'judged',
        entries: [
          { id: 'c1', text: 'Added `--resume-ask` to re-open the last question after resume' },
          { id: 'c2', text: 'Fixed statusLine `version` missing after /login' },
        ],
        verdicts: {
          verdicts: [
            { entry_id: 'c1', verdict: 'adopt', reason: '接回後停在提問的 bot 不用再靠畫面猜', module: 'lifecycle/start.rs' },
            { entry_id: 'c2', verdict: 'upgrade-arg', reason: '升級後 statusLine 版本不再掉，更新通知更準', module: 'update_watch.rs' },
          ],
          issues: [{ entry_ids: ['c1'], title: '接回時帶 --resume-ask，停在提問的 bot 直接重現選單（採用）', triage: 'adopt' }],
        },
        issues: [{ marker: 'm', entry_ids: ['c1'], number: 571, url: 'https://github.com/Eden-Sun/agents-manager/issues/571' }],
      },
      {
        kind, version: '2.1.281', status: 'judged',
        entries: [{ id: 'c3', text: 'Changed Stop hook payload: `last_assistant_message` is now truncated at 64k' }],
        verdicts: {
          verdicts: [{ entry_id: 'c3', verdict: 'guard', reason: '長回覆會被截斷，hook 配對要改讀 transcript', module: 'hookrecv.rs' }],
          issues: [{ entry_ids: ['c3'], title: 'Stop hook 的回覆被截在 64k，長回覆要改讀 transcript（提防）', triage: 'guard', duplicate_of: 540 }],
        },
        issues: [],
      },
    ]
  }
  return [
    {
      kind, version: '0.157.0', status: 'judged',
      entries: [
        { id: 'x1', text: 'Enabled fullscreen transcripts by default and added Shift-click to extend text selections.' },
        { id: 'x2', text: 'Markdown lists now render with Unicode bullets (•).' },
        { id: 'x3', text: 'Added an `f` shortcut to fork the conversation.' },
      ],
      verdicts: {
        verdicts: [
          { entry_id: 'x1', verdict: 'guard', reason: '全螢幕 transcript 會讓畫面判讀失效；start.rs 已固定帶 --no-alt-screen', module: 'lifecycle/start.rs' },
          { entry_id: 'x2', verdict: 'guard', reason: '回覆擷取會把最後一個清單項目當成回覆開頭', module: 'lifecycle/screen.rs' },
          { entry_id: 'x3', verdict: 'none', reason: 'daemon 不送單鍵 f', module: '' },
        ],
        issues: [
          { entry_ids: ['x1'], title: 'codex 0.157：全螢幕 transcript 預設開——畫面判讀要先固定（提防）', triage: 'guard', duplicate_of: 549 },
          { entry_ids: ['x2'], title: 'codex 0.157：Markdown 清單改用 •，畫面取回覆會被截斷（提防）', triage: 'guard' },
        ],
      },
      issues: [],
    },
    { kind, version: '0.156.1', status: 'empty', entries: [{ id: 'x4', text: 'Fixed resume picker ordering' }], verdicts: { verdicts: [{ entry_id: 'x4', verdict: 'none', reason: '不用 picker', module: '' }], issues: [] }, issues: [] },
    {
      kind, version: '0.156.0', status: 'judged',
      entries: [{ id: 'x5', text: '`codex exec --json` adds `turn.usage`' }],
      verdicts: { verdicts: [{ entry_id: 'x5', verdict: 'upgrade-arg', reason: 'exec 的用量可以直接讀，不必解析畫面', module: 'quota.rs' }], issues: [] },
      issues: [],
    },
  ]
}

export class MockReleaseTriage {
  private off = new Set<string>()
  private reviews = new Map<string, Rec>()

  triageOff(kind: string) {
    this.off.add(kind)
  }

  /** 認得的路徑回資料，不認得回 `undefined`（交回 mock 主體往下比對）。 */
  handle(method: string, rawPath: string, q: URLSearchParams, b: Rec): unknown {
    if (method === 'GET' && rawPath === '/changelog') {
      const kind = q.get('kind') ?? 'claude'
      const to = q.get('to') || DISK[kind] || ''
      const from = q.get('from')
      const all = CHANGELOG[kind] ?? []
      const sections = all.filter((s) => s.version <= to && (from ? s.version > from : s.version === to))
      return { kind, host: 'local', installed_version: to, from_version: from, found: sections.length > 0, sections, source_url: '', error: sections.length ? null : `沒有 ${to}` }
    }
    if (method === 'GET' && rawPath === '/release-triage') {
      const kind = q.get('kind') ?? 'claude'
      return { publish_enabled: false, repo: 'Eden-Sun/agents-manager', rows: this.off.has(kind) ? [] : triageRows(kind) }
    }
    if (rawPath === '/claude-update/review') {
      const kind = (method === 'GET' ? q.get('kind') : (b.kind as string | undefined)) ?? 'claude'
      const to = (method === 'GET' ? q.get('to') : (b.to as string | undefined)) || (kind === 'codex' ? '0.157.0' : DISK[kind])
      const key = `${kind}@${to}`
      if (method === 'POST' && !this.reviews.has(key)) {
        this.reviews.set(key, { state: 'pending', target_bot_name: 'AGM-responder', asked_at: new Date().toISOString() })
        return { kind, version: to, target_bot_name: 'AGM-responder', duplicate: false, review: this.reviews.get(key) }
      }
      const review = this.reviews.get(key) ?? { state: 'none' }
      return method === 'GET' ? { kind, version: to, review } : { kind, version: to, target_bot_name: 'AGM-responder', duplicate: true, review }
    }
    return undefined
  }
}
