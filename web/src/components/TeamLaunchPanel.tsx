import { useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import * as api from '../api'
import type { BotKind, IssueDetail, KindQuota, TeamBudget, TeamDeliver, TeamRoleSpec } from '../api/types'
import {
  BOT_KINDS,
  TEAM_BUDGET_DEFAULTS,
  TEAM_QUOTA_WARN_PCT,
  TEAM_WORKERS_DEFAULT,
  TEAM_WORKERS_MAX,
} from '../api/types'
import { projectHostName, toolsOfHost, useStore } from '../store/store'
import { IdentityOptions, PersonaField } from './BotSettingsPanel'
import { KindIcon, KindTag } from './KindTag'
import { ApiModelFields } from './ModelPicker'

/**
 * SPEC-team §11.2 — 「組隊」的設定 sheet（右側主區域，不是 modal；同 UI-DECISIONS 的
 * 「新增 Bot 表單改為 sheet」）。
 *
 * 三張角色卡（PM / 執行者 / Reviewer）各自選 kind 與模型（重用 `ApiModelFields`），
 * 執行者多一個 1–{TEAM_WORKERS_MAX} 的人數 stepper，Reviewer 可整張關掉（= 不審查直接合併）。
 *
 * 使用者裁決過的預設值：`deliver = branch`（**不是** §12 建議的 `pr`——`pr` 會 push 到
 * origin，所以那個選項在 UI 上有明確的紅字說明）、`supervised = false`、執行者上限 4、
 * 預算 40 / 2 / 120 / 90，且預算會用 localStorage 記住上次值。
 */

const BUDGET_KEY = 'am.teamBudget'

function readBudget(): TeamBudget {
  try {
    const raw = localStorage.getItem(BUDGET_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!parsed || typeof parsed !== 'object') return { ...TEAM_BUDGET_DEFAULTS }
    const o = parsed as Record<string, unknown>
    const one = (k: keyof TeamBudget) => (typeof o[k] === 'number' && Number.isFinite(o[k]) ? (o[k] as number) : TEAM_BUDGET_DEFAULTS[k])
    return {
      max_relays: one('max_relays'),
      max_review_rounds: one('max_review_rounds'),
      max_wall_clock_min: one('max_wall_clock_min'),
      quota_stop_pct: one('quota_stop_pct'),
    }
  } catch {
    return { ...TEAM_BUDGET_DEFAULTS }
  }
}

function writeBudget(b: TeamBudget) {
  try {
    localStorage.setItem(BUDGET_KEY, JSON.stringify(b))
  } catch {
    /* storage unavailable: the values still hold for this page */
  }
}

function emptySpec(kind: BotKind): TeamRoleSpec {
  return { kind, model: null, effort: null, fast: false, identity: null, persona_extra: '' }
}

/** IssuesBar 的同一套 label 配色。 */
function labelStyle(color: string | null) {
  if (!color) return undefined
  const r = parseInt(color.slice(0, 2), 16)
  const g = parseInt(color.slice(2, 4), 16)
  const b = parseInt(color.slice(4, 6), 16)
  const lum = (0.299 * r + 0.587 * g + 0.114 * b) / 255
  return { background: `#${color}`, color: lum > 0.6 ? '#1b1e23' : '#fff', borderColor: 'transparent' }
}

/** 某個 kind 目前最吃緊的視窗已用百分比；null = 沒有資料（不擋）。 */
function worstUsedPct(q: KindQuota | null | undefined): number | null {
  const vals = [q?.five_hour?.used_pct, q?.seven_day?.used_pct].filter((v): v is number => typeof v === 'number')
  return vals.length ? Math.max(...vals) : null
}

/**
 * §9.2 的額度預覽。用 `QuotaStrip` 同一組 class（`quota-hp` / `quota-bars` / `quota-bar`），
 * 顯示的是**剩餘**，和頂欄的量條一致。
 */
function QuotaPill({ kind, identity, stopPct }: { kind: BotKind; identity: string | null; stopPct: number }) {
  const key = identity ? `${kind}:${identity}` : kind
  const q = useStore((s) => s.quota[key] ?? s.quota[kind] ?? null)
  const used = worstUsedPct(q)
  const left = used === null ? null : Math.max(0, Math.round(100 - used))
  const level = used === null ? 'ok' : used >= stopPct ? 'crit' : used >= TEAM_QUOTA_WARN_PCT ? 'warn' : 'ok'
  const title =
    used === null
      ? `${kind}${identity ? ` · ${identity}` : ''}：尚未取得額度資訊（不會擋建立）`
      : `${kind}${identity ? ` · ${identity}` : ''}：最吃緊的視窗已用 ${Math.round(used)}%，剩 ${left}%`
  return (
    <span className={`quota-hp ${kind} ${level} team-quota-pill`} title={title}>
      <span className="quota-kind" aria-hidden="true">
        <KindIcon kind={kind} />
      </span>
      {identity ? (
        <span className="quota-identity" aria-hidden="true">
          {identity}
        </span>
      ) : null}
      <span className="quota-bars">
        <span className={`quota-bar ${level}${left === null ? ' nodata' : ''}`}>
          <span className="quota-bar-fill" style={{ width: left === null ? '0%' : `${left}%` }} />
        </span>
      </span>
      <span className="team-quota-num">{left === null ? '—' : `${left}%`}</span>
    </span>
  )
}

function RoleCard({
  title,
  hint,
  host,
  spec,
  onSpec,
  disabled,
  head,
  extra,
}: {
  title: string
  hint: string
  host: string
  spec: TeamRoleSpec
  onSpec: (next: TeamRoleSpec) => void
  disabled?: boolean
  /** 卡片標題列右側（執行者的人數 stepper / Reviewer 的開關）。 */
  head?: ReactNode
  extra?: ReactNode
}) {
  const tools = useStore((s) => toolsOfHost(s, host))
  const patch = (p: Partial<TeamRoleSpec>) => onSpec({ ...spec, ...p })

  return (
    <section className={`team-card${disabled ? ' off' : ''}`}>
      <header className="team-card-head">
        <span className="team-card-title">{title}</span>
        {head}
      </header>
      <p className="team-card-hint">{hint}</p>
      {disabled ? null : (
        <>
          <div className="field">
            <span>kind</span>
            <div className="opt-group kinds" role="radiogroup" aria-label={`${title} kind`}>
              {BOT_KINDS.map((k) => {
                const missing = !tools[k].installed
                return (
                  <button
                    key={k}
                    type="button"
                    role="radio"
                    aria-checked={spec.kind === k}
                    className={`opt${spec.kind === k ? ' on' : ''}`}
                    disabled={missing}
                    title={missing ? `${host === 'local' ? '本機' : host} 尚未安裝 ${k}` : k}
                    onClick={() => onSpec({ ...emptySpec(k), persona_extra: spec.persona_extra })}
                  >
                    <KindTag kind={k} />
                    <span className="opt-label">{k}</span>
                    {missing ? <span className="kind-missing-reason">未安裝</span> : null}
                  </button>
                )
              })}
            </div>
          </div>
          <ApiModelFields
            kind={spec.kind}
            host={host}
            model={spec.model}
            onModel={(v) => patch({ model: v })}
            effort={spec.effort}
            onEffort={(v) => patch({ effort: v })}
            fast={spec.fast}
            onFast={(v) => patch({ fast: v })}
          />
          <IdentityOptions kind={spec.kind} value={spec.identity ?? ''} onChange={(v) => patch({ identity: v || null })} />
          {extra}
          <PersonaField value={spec.persona_extra} onChange={(v) => patch({ persona_extra: v })} collapsible />
        </>
      )}
    </section>
  )
}

function NumberField({
  label,
  note,
  value,
  min,
  max,
  onChange,
}: {
  label: string
  note: string
  value: number
  min: number
  max: number
  onChange: (v: number) => void
}) {
  return (
    <label className="field team-budget-field">
      <span>
        {label}
        <span className="field-note">{note}</span>
      </span>
      <input
        type="number"
        min={min}
        max={max}
        value={value}
        onChange={(e) => {
          const n = Number(e.target.value)
          if (Number.isFinite(n)) onChange(Math.max(min, Math.min(max, Math.round(n))))
        }}
      />
    </label>
  )
}

export function TeamLaunchPanel({
  projectId,
  issueNumber,
  onOpenSidebar,
}: {
  projectId: string
  issueNumber: number
  onOpenSidebar: () => void
}) {
  const project = useStore((s) => s.projects.find((p) => p.id === projectId) ?? null)
  const host = useStore((s) => projectHostName(s, projectId))
  const tools = useStore((s) => toolsOfHost(s, host))
  const quota = useStore((s) => s.quota)
  const createTeam = useStore((s) => s.createTeam)
  const closeTeamLaunch = useStore((s) => s.closeTeamLaunch)
  const notify = useStore((s) => s.notify)

  const firstKind = useMemo<BotKind>(() => BOT_KINDS.find((k) => tools[k].installed) ?? 'claude', [tools])
  const [pm, setPm] = useState<TeamRoleSpec>(() => emptySpec(firstKind))
  const [worker, setWorker] = useState<TeamRoleSpec>(() => emptySpec(firstKind))
  const [reviewer, setReviewer] = useState<TeamRoleSpec>(() => emptySpec(firstKind))
  const [count, setCount] = useState(TEAM_WORKERS_DEFAULT)
  const [hasReviewer, setHasReviewer] = useState(true)
  const [base, setBase] = useState('HEAD')
  const [deliver, setDeliver] = useState<TeamDeliver>('branch')
  const [supervised, setSupervised] = useState(false)
  const [budget, setBudget] = useState<TeamBudget>(readBudget)
  const [issue, setIssue] = useState<IssueDetail | null>(null)
  const [issueError, setIssueError] = useState<string | null>(null)
  const [bodyOpen, setBodyOpen] = useState(false)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let alive = true
    api
      .fetchIssue(projectId, issueNumber)
      .then((d) => alive && setIssue(d))
      .catch((e: unknown) => alive && setIssueError(e instanceof Error ? e.message : String(e)))
    return () => {
      alive = false
    }
  }, [projectId, issueNumber])

  const roles = useMemo(
    () => [
      { label: 'PM', spec: pm },
      { label: '執行者', spec: worker },
      ...(hasReviewer ? [{ label: 'Reviewer', spec: reviewer }] : []),
    ],
    [pm, worker, reviewer, hasReviewer],
  )

  /** §9.2：任一角色所選 kind 的已用量 ≥ quota_stop_pct → 不給建立。 */
  const blockedKind = roles.find((r) => {
    const key = r.spec.identity ? `${r.spec.kind}:${r.spec.identity}` : r.spec.kind
    const used = worstUsedPct(quota[key] ?? quota[r.spec.kind])
    return used !== null && used >= budget.quota_stop_pct
  })
  const missingCli = roles.find((r) => !tools[r.spec.kind].installed)
  const ghReady = Boolean(project?.github)
  const canSubmit = !busy && !blockedKind && !missingCli && Boolean(project)

  const submit = () => {
    if (!canSubmit) return
    setBusy(true)
    writeBudget(budget)
    void createTeam(projectId, {
      issue_number: issueNumber,
      pm,
      workers: { ...worker, count },
      reviewer: hasReviewer ? reviewer : null,
      base: base.trim() || 'HEAD',
      deliver,
      supervised,
      budget,
    }).finally(() => setBusy(false))
  }

  if (!project) {
    return (
      <div className="main-head">
        <span className="main-status">找不到 Project</span>
      </div>
    )
  }

  return (
    <>
      <div className="main-head team-head">
        <button
          type="button"
          className="btn menu-btn icon-tip"
          onClick={onOpenSidebar}
          aria-label="開啟側邊欄"
          title="開啟側邊欄"
          data-tip="開啟側邊欄"
        >
          ☰
        </button>
        <div className="main-title">
          <span className="team-icon" aria-hidden="true">
            ⚙
          </span>
          <strong>組隊</strong>
          <span className="team-tag">#{issueNumber}</span>
        </div>
        <span className="spacer" />
        <span className="main-status" title={project.path}>
          {project.label}
        </span>
        <div className="head-actions">
          <button type="button" className="mini-btn" onClick={closeTeamLaunch} title="不建立 team，回到原本的畫面">
            取消
          </button>
        </div>
      </div>

      <div className="team-launch">
        <section className="team-issue">
          <div className="team-issue-head">
            <a className="issue-title" href={issue?.url ?? project.github?.url} target="_blank" rel="noreferrer">
              <span className="issue-num">#{issueNumber}</span> {issue?.title ?? (issueError ? '（讀取失敗）' : '讀取中…')}
            </a>
            <div className="issue-meta">
              {(issue?.labels ?? []).map((l) => (
                <span key={l.name} className="issue-label" style={labelStyle(l.color)}>
                  {l.name}
                </span>
              ))}
              {issue?.author ? <span className="issue-author">{issue.author}</span> : null}
            </div>
          </div>
          {issueError ? <p className="hint">讀不到 issue 內容：{issueError}（仍可建立，daemon 會自己再讀一次）</p> : null}
          {issue?.body ? (
            <>
              <button
                type="button"
                className="disclosure sub"
                aria-expanded={bodyOpen}
                onClick={() => setBodyOpen((v) => !v)}
              >
                <span className="chev">{bodyOpen ? '▼' : '▶'}</span> issue 全文
              </button>
              {bodyOpen ? <pre className="team-issue-body">{issue.body}</pre> : null}
            </>
          ) : null}
        </section>

        <div className="team-cards">
          <RoleCard
            title="PM"
            hint="拆 task、派工、收回報，不寫程式、不 commit。cwd 是整合分支的 worktree。"
            host={host}
            spec={pm}
            onSpec={setPm}
          />
          <RoleCard
            title="執行者"
            hint={`每人一個獨立 worktree 與分支；同一組設定套用到全部 ${count} 人。`}
            host={host}
            spec={worker}
            onSpec={setWorker}
            head={
              <span className="team-count" role="group" aria-label="執行者人數">
                <button
                  type="button"
                  className="mini-btn"
                  disabled={count <= 1}
                  aria-label="減少一位執行者"
                  onClick={() => setCount((c) => Math.max(1, c - 1))}
                >
                  −
                </button>
                <span className="team-count-num" aria-live="polite">
                  {count} 人
                </span>
                <button
                  type="button"
                  className="mini-btn"
                  disabled={count >= TEAM_WORKERS_MAX}
                  aria-label="增加一位執行者"
                  title={count >= TEAM_WORKERS_MAX ? `上限 ${TEAM_WORKERS_MAX} 人（reviewer 序列化審查，再多只會排隊）` : undefined}
                  onClick={() => setCount((c) => Math.min(TEAM_WORKERS_MAX, c + 1))}
                >
                  ＋
                </button>
              </span>
            }
          />
          <RoleCard
            title="Reviewer"
            hint={hasReviewer ? '唯讀審查通過的才會被合併；打回票會讓執行者重做一輪。' : '不審查：執行者回報 done 就直接合併進整合分支。'}
            host={host}
            spec={reviewer}
            onSpec={setReviewer}
            disabled={!hasReviewer}
            head={
              <label className="team-toggle">
                <input type="checkbox" checked={hasReviewer} onChange={(e) => setHasReviewer(e.target.checked)} />
                <span>要審查</span>
              </label>
            }
          />
        </div>

        <section className="team-card team-options">
          <header className="team-card-head">
            <span className="team-card-title">交付與預算</span>
          </header>

          <div className="field">
            <span>交付方式</span>
            <div className="opt-group" role="radiogroup" aria-label="交付方式">
              <button
                type="button"
                role="radio"
                aria-checked={deliver === 'branch'}
                className={`opt${deliver === 'branch' ? ' on' : ''}`}
                title="整合分支留在本地 repo，不 push"
                onClick={() => setDeliver('branch')}
              >
                留分支
                <span className="opt-note">預設</span>
              </button>
              <button
                type="button"
                role="radio"
                aria-checked={deliver === 'pr'}
                className={`opt danger-opt${deliver === 'pr' ? ' on' : ''}`}
                disabled={!ghReady}
                title={ghReady ? '會 git push -u origin 並用 gh 開 PR' : '這個 Project 沒有 GitHub remote'}
                onClick={() => setDeliver('pr')}
              >
                開 PR
                <span className="opt-note warn">會 push 到 origin</span>
              </button>
            </div>
            <span className={`hint${deliver === 'pr' ? ' warn-hint' : ''}`}>
              {deliver === 'pr'
                ? `完成時會執行 git push -u origin <整合分支> 並用 gh 開 PR（Closes #${issueNumber}）——內容會出現在 ${project.github?.owner}/${project.github?.repo} 的遠端。`
                : '整合分支只留在本機 repo，不會 push；要開 PR 你自己來。'}
            </span>
          </div>

          <label className="field row">
            <input type="checkbox" checked={supervised} onChange={(e) => setSupervised(e.target.checked)} />
            <span>
              supervised（逐步放行）
              <span className="field-note">派工 / 合併 / 交付前各停一次等你按放行。第二階段才有完整 UI，預設關。</span>
            </span>
          </label>

          <label className="field">
            <span>
              base
              <span className="field-note">整合分支從哪個 ref 開出來</span>
            </span>
            <input type="text" value={base} spellCheck={false} placeholder="HEAD" onChange={(e) => setBase(e.target.value)} />
          </label>

          <div className="team-budget">
            <NumberField
              label="轉送上限"
              note="max_relays：所有 bot 間轉送的總次數"
              value={budget.max_relays}
              min={1}
              max={500}
              onChange={(v) => setBudget((b) => ({ ...b, max_relays: v }))}
            />
            <NumberField
              label="審查回合"
              note="max_review_rounds：每件 task 最多被打回幾次"
              value={budget.max_review_rounds}
              min={0}
              max={10}
              onChange={(v) => setBudget((b) => ({ ...b, max_review_rounds: v }))}
            />
            <NumberField
              label="時間上限"
              note="max_wall_clock_min：分鐘"
              value={budget.max_wall_clock_min}
              min={5}
              max={1440}
              onChange={(v) => setBudget((b) => ({ ...b, max_wall_clock_min: v }))}
            />
            <NumberField
              label="額度停手線"
              note="quota_stop_pct：任一視窗已用達此 % 就暫停"
              value={budget.quota_stop_pct}
              min={10}
              max={100}
              onChange={(v) => setBudget((b) => ({ ...b, quota_stop_pct: v }))}
            />
          </div>
          <span className="hint">上限只會讓 team「暫停」，不會中止；暫停後可以加碼再繼續。這些數字會記住下次沿用。</span>
        </section>

        <section className="team-card team-quota">
          <header className="team-card-head">
            <span className="team-card-title">額度預覽</span>
            <span className="team-card-note">
              最多約 {budget.max_relays} 次轉送 × {count + 1 + (hasReviewer ? 1 : 0)} 位成員
            </span>
          </header>
          <div className="team-quota-row">
            {roles.map((r) => (
              <span key={r.label} className="team-quota-item">
                <span className="team-quota-role">{r.label}</span>
                <QuotaPill kind={r.spec.kind} identity={r.spec.identity} stopPct={budget.quota_stop_pct} />
              </span>
            ))}
          </div>
          {blockedKind ? (
            <p className="team-block" role="alert">
              {blockedKind.label} 用的 {blockedKind.spec.kind} 額度已達停手線（{budget.quota_stop_pct}%），現在建立 team 會馬上暫停。
              請換 kind / 身份，或把停手線調高。
            </p>
          ) : null}
          {missingCli ? (
            <p className="team-block" role="alert">
              {missingCli.label} 選的 {missingCli.spec.kind} 在{host === 'local' ? '本機' : host}尚未安裝。
            </p>
          ) : null}
        </section>

        <div className="team-launch-actions">
          <button type="button" className="btn" onClick={closeTeamLaunch}>
            取消
          </button>
          <button
            type="button"
            className="btn primary"
            disabled={!canSubmit}
            title={
              deliver === 'pr'
                ? '建立 team 並啟動成員；完成時會 push 分支並開 PR'
                : '建立 team 並啟動成員；完成時只留下整合分支'
            }
            onClick={() => {
              if (deliver === 'pr' && !ghReady) {
                notify('error', '這個 Project 沒有 GitHub remote，無法開 PR。')
                return
              }
              submit()
            }}
          >
            {busy ? '建立中…' : `建立並啟動（${count + 1 + (hasReviewer ? 1 : 0)} 位成員）`}
          </button>
        </div>
      </div>
    </>
  )
}
