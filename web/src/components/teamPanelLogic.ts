import type { Team, TeamEvent, TeamPauseQuotaMember } from '../api/types.ts'
import { TEAM_PHASE_LABEL, TEAM_ROLE_LABEL, teamPauseLabel, teamShortName } from '../api/types.ts'

/**
 * TeamPanel 裡「純算字串」的那一半。抽出來的理由是它可以單獨用
 * `node --test --experimental-strip-types` 跑（元件本身不行），所以這個檔案只准
 * import `api/types.ts`——而且要帶副檔名，node 的 resolver 不會自己補。
 */

/**
 * SPEC-team §2.5.5：done 的 team 只要還沒 cleanup 就能追加 issue 繼續。
 *
 * 「還沒 cleanup」的判準跟 daemon 同一條（§2.5.1）：PM 成員的 bot 還在。`cleanup` 與
 * `delete` 都會把成員軟刪除，`team_json` 用 `deleted` 把這件事送上來。`unavailable` 是
 * daemon 已經回過 409 `team is cleaned up` 的記憶——狀態還沒重新載進來之前先把按鈕收掉，
 * 免得使用者再按一次拿到同一個錯誤。
 */
export function canReopenTeam(team: Team | null, unavailable = false): boolean {
  if (!team || unavailable || team.phase !== 'done') return false
  return team.members.some((member) => member.role === 'pm' && member.deleted !== true)
}

/** `finishing` → `收尾中`；不認得的（或空的）就原樣回去，總比空白好。 */
function phaseLabel(v: unknown): string {
  const raw = typeof v === 'string' ? v : ''
  return (TEAM_PHASE_LABEL as Record<string, string>)[raw] ?? raw
}

/**
 * `note` 事件的人話。
 *
 * 這些 payload 本來是 daemon 寫給自己的記錄，時間軸卻直接把 `action：k=v` 印出來
 * （`note reply_ok：role=pm`、`note issue_finished：branch=…，seq=6`）。時間軸是給人看的，
 * 所以：常見的記帳事件翻成一句話；純粹重複旁邊那則訊息的（`reply_ok`）直接不顯示；
 * 其餘（多半是失敗）保留 `action：欄位` 的原樣——那些欄位就是你要拿去查的東西。
 */
function noteText(p: Record<string, unknown>): string | null {
  const str = (key: string): string => (typeof p[key] === 'string' ? String(p[key]) : '')
  const num = (key: string): string => (typeof p[key] === 'number' ? String(p[key]) : '')
  const action = str('action')
  if (!action) return null
  // note 的 payload 帶的是完整成員名（`ttxka1d-i2-rev`）；畫面上其他地方一律用短名。
  const bot = teamShortName(str('bot'))
  const error = str('error')
  const issue = num('issue_number')
  const numberList = (key: string): string =>
    Array.isArray(p[key]) ? (p[key] as unknown[]).map((n) => `#${String(n)}`).join('、') : ''
  switch (action) {
    // 成員回覆本身就在時間軸上、就在這一列旁邊，再記一次「他回了」是純重複。
    case 'reply_ok':
      return null
    case 'member_start_failed':
      return `成員 ${bot} 啟動失敗：${error}`
    case 'pretrust_failed':
      return `無法預先信任工作目錄：${error}`
    case 'protocol_error':
      return `${bot} 的回覆沒有可用的 am-team 區塊（第 ${num('attempt') || 1} 次）：${error}`
    case 'issue_closed': {
      const number = num('number')
      return p.already_closed === true ? `issue #${number} 本來就已經關閉` : `已關閉 issue #${number}`
    }
    case 'issues_queued': {
      const list = numberList('issue_numbers')
      return list ? `已排入佇列：${list}` : '已排入佇列'
    }
    // SPEC-team §2.5：done 的 team 被使用者追加 issue 拉回佇列流程。
    case 'team_reopened': {
      const list = numberList('issue_numbers')
      return list ? `使用者追加 ${list}，team 重新啟動` : '使用者追加 issue，team 重新啟動'
    }
    // §2.5.3：CLI 沒能續接原本那段對話。哪個角色比 bot 名字重要——使用者關心的是
    // 「PM 還記不記得上一個 issue」，`why` 是給查問題的人看的，留在 payload 裡就好。
    case 'member_context_lost': {
      const role = str('role')
      const who = role === 'pm' ? 'PM' : ((TEAM_ROLE_LABEL as Record<string, string>)[role] ?? (bot || '成員'))
      return `${who} 沒能續接先前對話，已改為新對話`
    }
    case 'issue_unqueued':
      return `已從佇列移除 issue #${issue}`
    case 'issue_started':
      return `開始處理 issue #${issue}${num('seq') ? `（佇列第 ${num('seq')} 個）` : ''}`
    case 'issue_finished':
      return `issue #${issue} 完成`
    case 'issue_failed':
      return `issue #${issue} 失敗${str('reason') ? `（${teamPauseLabel(str('reason'))}）` : ''}`
    case 'issue_start_failed':
      return `issue #${issue} 啟動失敗：${error}`
    case 'gate':
      return `等你放行：${str('gate')}`
    case 'gate_release':
      return `已放行：${str('gate')}`
    case 'abort':
      return `已中止${str('reason') ? `（${teamPauseLabel(str('reason'))}）` : ''}`
    case 'pm_abort':
      return `PM 要求中止${str('reason') ? `（${teamPauseLabel(str('reason'))}）` : ''}`
    case 'worker_plan':
      return p.keep === true ? '下一個 issue 沿用同一批執行者' : '下一個 issue 換一批執行者'
    case 'pm_repeat':
      return `PM 又把同一件事派給 ${teamShortName(str('to'))}`
    case 'auto_commit':
      return `已幫 ${bot} 把沒提交的變更 commit`
    case 'pr_created':
      return `已 push ${str('pushed')} 並開 PR`
    case 'deliver_downgraded':
      return '這個 project 沒有 GitHub origin，改成只留整合分支'
    case 'delivered':
      return str('deliver') === 'pr' ? '已交付：PR' : `已交付：留下整合分支 ${str('branch')}`
    case 'cleanup':
      return '已清理：移除成員與 worktree（分支保留）'
    case 'patch':
      return '設定已更新'
    default: {
      // 不認得的（幾乎都是失敗）照舊把欄位攤開——那是唯一能查下去的線索。
      const rest = Object.entries(p)
        .filter(([key, value]) => key !== 'action' && (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean'))
        .map(([key, value]) => `${key}=${String(value)}`)
        .join('，')
      return rest ? `${action}：${rest}` : action
    }
  }
}

/** 時間軸上一則 team 事件的人話；`null` = 這一列不畫。 */
export function describeEvent(ev: TeamEvent): string | null {
  const p = ev.payload
  if (ev.kind === 'merge') {
    // 左邊那顆小標已經寫著「合併」，這裡只說結果——否則整列讀起來是「合併 合併衝突：…」。
    const branch = typeof p.branch === 'string' ? p.branch : ''
    if (p.result === 'ok') return `完成：${branch}${typeof p.sha === 'string' ? `（${p.sha}）` : ''}`
    const files = Array.isArray(p.conflict_files) ? p.conflict_files.join('、') : ''
    return `衝突：${branch}${files ? ` — ${files}` : ''}`
  }
  if (ev.kind === 'phase') {
    // 階段代號（`finishing → done`）是 daemon 的字，時間軸是給人看的：兩邊都翻成中文。
    const to = phaseLabel(p.to)
    const from = phaseLabel(p.from)
    const reason = typeof p.reason === 'string' && p.reason ? `（${teamPauseLabel(p.reason)}）` : ''
    return `${from} → ${to}${reason}`
  }
  if (ev.kind === 'note') {
    if (typeof p.text === 'string') return p.text
    return noteText(p)
  }
  return null
}

/**
 * 暫停中的隊伍在**側欄**能當場做什麼（`TeamNodes`）。TeamPanel 標題列本來就有這幾顆按鈕，
 * 但要先點進面板才看得到——#53 停在 `budget_time` 兩個多小時，側欄只有一顆褐色點，
 * 使用者以為是卡死。
 *
 * - `bump`：預算類的暫停，加碼（各 ×2）之後直接繼續，同標題列的「加碼預算」＋「繼續」。
 * - `resume`：原因處理完就能推，側欄給一顆「繼續」。
 * - `null`：得進面板才處理得掉（要回答 PM、要放行閘門、要先救成員），側欄只寫原因。
 */
export type TeamPauseAction = 'bump' | 'resume' | null

export function teamPauseAction(reason: string | null): TeamPauseAction {
  if (reason === 'budget_time' || reason === 'budget_relays') return 'bump'
  if (!reason) return 'resume'
  if (reason === 'ask_user' || reason.startsWith('gate:') || reason.startsWith('member_')) return null
  return 'resume'
}

/**
 * `quota_low` 的橫幅要**指名道姓**（2026-09-09）。
 *
 * 原本只寫「已暫停：額度過低」——隊伍裡有 pm / dev-1 / dev-2 / rev 四個成員、各自可能掛在
 * 不同帳號（`cc0` / `cc2` / codex）上，使用者看不出要去處理哪一個身分，只能一個一個點開
 * 成員的 pane 對額度。daemon 現在把撞到上限的成員連同視窗、剩餘 % 與 reset 時間放進
 * `pause_detail`（SPEC-team §4.5），這裡把它排成一句話。
 */
function quotaWindowLabel(m: TeamPauseQuotaMember): string {
  // grok 的長週期是「每週」不是「7 天」，跟額度列（QuotaStrip）用同一套說法。
  if (m.window === 'seven_day') return m.kind === 'grok' ? '週' : '7d'
  return '5h'
}

/** `2026-09-09T03:20:00Z` → `03:20`；今天以外的還要日期，不然「03:20」會被讀成再幾分鐘。 */
function quotaResetLabel(iso: string | null): string {
  if (!iso) return ''
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  const now = new Date()
  const sameDay =
    d.getFullYear() === now.getFullYear() && d.getMonth() === now.getMonth() && d.getDate() === now.getDate()
  return d.toLocaleString([], {
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
    ...(sameDay ? {} : { month: '2-digit', day: '2-digit' }),
  })
}

/** `rev（cc2）5h 額度剩 4%，03:20 才 reset`。 */
export function teamQuotaMemberText(m: TeamPauseQuotaMember): string {
  const who = m.identity ? `${m.short}（${m.identity}）` : `${m.short}（${m.kind}）`
  const pct = Math.round(m.remaining_pct)
  const reset = quotaResetLabel(m.resets_at)
  // `）` 本來就佔滿一格，後面再補空格會在橫幅上開一個洞。
  return `${who}${quotaWindowLabel(m)} 額度剩 ${pct}%${reset ? `，${reset} 才 reset` : ''}`
}

/**
 * 橫幅裡「已暫停：」後面那一段。`quota_low` 以外的原因照舊用機器碼的中文。
 *
 * 兩位以上的話列最嚴重的那個 + 「另有 N 位」——四個成員全列會把橫幅撐成一段文章，
 * 而全部名單在 `title` 裡（見 `teamPauseDetailTitle`）。
 */
export function teamPauseText(team: Team): string {
  const detail = team.pause_reason === 'quota_low' ? team.pause_detail : null
  if (!detail?.members.length) return teamPauseLabel(team.pause_reason)
  const [worst, ...rest] = detail.members
  return `${teamQuotaMemberText(worst)}${rest.length ? `（另有 ${rest.length} 位額度也不足）` : ''}`
}

/** 全部額度不足的成員，一行一個；沒有細節就是空字串。 */
export function teamPauseDetailLines(team: Team): string {
  const detail = team.pause_reason === 'quota_low' ? team.pause_detail : null
  if (!detail) return ''
  return detail.members.map((m) => teamQuotaMemberText(m)).join('\n')
}

/** 橫幅的 `title`——只有一位時 tooltip 不會比橫幅本身多講什麼，就不掛。 */
export function teamPauseDetailTitle(team: Team): string | undefined {
  const detail = team.pause_reason === 'quota_low' ? team.pause_detail : null
  if (!detail || detail.members.length < 2) return undefined
  return teamPauseDetailLines(team)
}
