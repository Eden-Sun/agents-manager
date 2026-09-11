import { useCallback, useEffect, useId, useRef, useState } from 'react'
import type { RefObject } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import type { Issue, ProjectSubmodule } from '../api/types'
import { useStore } from '../store/store'
import type { DraftKey } from '../store/store'
import { GhLoginButton, isGhAuthError } from './GhAuth'
import { onTabListKeyDown } from './tabKeys'

/**
 * v4.0 GitHub issues (`GET /api/projects/:id/issues`, via `gh` on the daemon). A thin bar
 * under the header with one button (`owner/repo · N open`) that opens a panel: search
 * (300 ms debounce → `q`), open / closed toggle (remembered), the list, and per row
 * 「插入」(`#n title\nurl` at the composer caret) / 「插入完整內容」(body as a `> ` quote).
 */

const STATE_KEY = 'am.issueState'
type IssueState = 'open' | 'closed'

function readState(): IssueState {
  try {
    return localStorage.getItem(STATE_KEY) === 'closed' ? 'closed' : 'open'
  } catch {
    return 'open'
  }
}

function ago(iso: string): string {
  const t = new Date(iso).getTime()
  if (Number.isNaN(t)) return ''
  const m = Math.max(0, Math.round((Date.now() - t) / 60000))
  if (m < 60) return `${m} 分鐘前`
  const h = Math.round(m / 60)
  if (h < 48) return `${h} 小時前`
  return `${Math.round(h / 24)} 天前`
}

/** Readable text colour for a GitHub label hex. */
function labelStyle(color: string | null) {
  if (!color) return undefined
  const r = parseInt(color.slice(0, 2), 16)
  const g = parseInt(color.slice(2, 4), 16)
  const b = parseInt(color.slice(4, 6), 16)
  const lum = (0.299 * r + 0.587 * g + 0.114 * b) / 255
  return { background: `#${color}`, color: lum > 0.6 ? '#1b1e23' : '#fff', borderColor: 'transparent' }
}

function errorText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 502) return `gh 無法使用：${e.message}`
    return `${e.message}（HTTP ${e.status}）`
  }
  return e instanceof Error ? e.message : String(e)
}

export function IssuesBar({ projectId, draftKey, inputRef }: { projectId: string; draftKey: DraftKey; inputRef: RefObject<HTMLTextAreaElement | null> }) {
  const github = useStore((s) => s.projects.find((p) => p.id === projectId)?.github ?? null)
  const host = useStore((s) => s.projects.find((p) => p.id === projectId)?.host ?? 'local')
  const setDraft = useStore((s) => s.setDraft)
  const setDraftCursor = useStore((s) => s.setDraftCursor)
  const notify = useStore((s) => s.notify)
  // SPEC-team §11.1：舊 daemon 沒有 team 端點時 `teamsSupported` 會翻成 false，這顆按鈕靜默消失。
  const teamsSupported = useStore((s) => s.teamsSupported)
  const openTeamLaunch = useStore((s) => s.openTeamLaunch)
  const [open, setOpen] = useState(false)
  const [state, setState] = useState<IssueState>(readState)
  const [q, setQ] = useState('')
  const [issues, setIssues] = useState<Issue[] | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [authError, setAuthError] = useState(false)
  const [openCount, setOpenCount] = useState<number | null>(null)
  const [fetching, setFetching] = useState<number | null>(null)
  // 專案的 submodule（有自己 GitHub origin 的才能選）與目前選的那個：`''` = 專案本身。
  // 兩者都掛在 projectId 上，換專案就自然歸零，不用在 effect 裡 setState。
  const [subsFor, setSubsFor] = useState<{ pid: string; subs: ProjectSubmodule[] }>({ pid: '', subs: [] })
  const [repoFor, setRepoFor] = useState<{ pid: string; repo: string }>({ pid: '', repo: '' })
  const submodules = subsFor.pid === projectId ? subsFor.subs : []
  const repo = repoFor.pid === projectId ? repoFor.repo : ''
  const setRepo = (r: string) => setRepoFor({ pid: projectId, repo: r })
  const wrap = useRef<HTMLDivElement>(null)
  const searchRef = useRef<HTMLInputElement>(null)
  const seq = useRef(0)
  const tabsId = useId()

  // Submodules once per project; the picker only appears when at least one is on GitHub.
  useEffect(() => {
    if (!github) return
    let alive = true
    api
      .fetchSubmodules(projectId)
      .then((subs) => alive && setSubsFor({ pid: projectId, subs: subs.filter((s) => s.github) }))
      .catch(() => alive && setSubsFor({ pid: projectId, subs: [] }))
    return () => {
      alive = false
    }
  }, [projectId, github])

  // Open-issue count for the button (once per project and repo).
  useEffect(() => {
    if (!github) return
    let alive = true
    api
      .fetchIssues(projectId, { state: 'open', limit: 100, repo })
      .then((list) => alive && setOpenCount(list.length))
      .catch(() => alive && setOpenCount(null))
    return () => {
      alive = false
    }
  }, [projectId, github, repo])

  const load = useCallback(
    async (st: IssueState, query: string) => {
      const id = ++seq.current
      setLoading(true)
      setError(null)
      setAuthError(false)
      try {
        const list = await api.fetchIssues(projectId, { state: st, limit: 50, q: query.trim() || undefined, repo })
        if (id !== seq.current) return
        setIssues(list)
        if (st === 'open' && !query.trim()) setOpenCount(list.length)
      } catch (e) {
        if (id !== seq.current) return
        setIssues(null)
        setAuthError(isGhAuthError(e))
        setError(errorText(e))
      } finally {
        if (id === seq.current) setLoading(false)
      }
    },
    [projectId, repo],
  )

  const retryIssues = useCallback(() => {
    void load(state, q)
  }, [load, state, q])

  // Query changes are debounced 300 ms; the state toggle refetches immediately.
  useEffect(() => {
    if (!open) return
    const t = setTimeout(() => void load(state, q), q ? 300 : 0)
    return () => clearTimeout(t)
  }, [open, state, q, load])

  useEffect(() => {
    if (!open) return
    searchRef.current?.focus()
    const onDoc = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  if (!github) return null

  // 按鈕上顯示目前選的 repo（submodule 的 slug 或專案自己的）。
  const shown = submodules.find((s) => s.path === repo)?.github ?? github

  const pickState = (st: IssueState) => {
    setState(st)
    try {
      localStorage.setItem(STATE_KEY, st)
    } catch {
      /* ignore */
    }
  }

  /** Insert at the composer caret (or the end), then put the caret after the snippet. */
  const insert = (snippet: string) => {
    const el = inputRef.current
    const cur = useStore.getState().drafts[draftKey] ?? ''
    const start = el && el.selectionStart !== null ? el.selectionStart : cur.length
    const end = el && el.selectionEnd !== null ? el.selectionEnd : start
    const before = cur.slice(0, start)
    const after = cur.slice(end)
    const pad = before && !before.endsWith('\n') ? '\n' : ''
    const tail = after && !after.startsWith('\n') ? '\n' : ''
    const text = `${before}${pad}${snippet}${tail}${after}`
    setDraft(draftKey, text)
    const pos = before.length + pad.length + snippet.length
    setDraftCursor(draftKey, pos)
    requestAnimationFrame(() => {
      const ta = inputRef.current
      if (!ta) return
      ta.focus()
      ta.setSelectionRange(pos, pos)
    })
    setOpen(false)
  }

  const insertRef = (i: Issue) => insert(`#${i.number} ${i.title}\n${i.url}`)

  const insertFull = async (i: Issue) => {
    setFetching(i.number)
    try {
      const d = await api.fetchIssue(projectId, i.number, repo)
      const body = (d?.body ?? '').trim()
      const quoted = body ? body.split('\n').map((l) => `> ${l}`).join('\n') : '> （沒有內容）'
      insert(`#${i.number} ${i.title}\n${i.url}\n${quoted}`)
    } catch (e) {
      notify('error', `讀取 #${i.number} 失敗：${errorText(e)}`)
    } finally {
      setFetching(null)
    }
  }

  return (
    <div className="issues-bar" ref={wrap}>
      <button
        type="button"
        className={`issues-btn${open ? ' on' : ''}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        title={`${github.url}/issues — 點開搜尋並插入 issue 到輸入框`}
        onClick={() => setOpen(!open)}
      >
        <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
          <circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" strokeWidth="1.5" />
          <circle cx="8" cy="8" r="2" fill="currentColor" />
        </svg>
        <span className="issues-repo">
          {/* owner 在手機上是這一列最先讓位的東西：同一畫面沒有第二個 repo 可混淆。 */}
          <span className="issues-owner">{shown.owner}/</span>
          {shown.repo}
        </span>
        {/* 數字還沒回來時本來會顯示 `Issues`——一個長得像計數的藥丸裡放一個單字，讀起來
            像壞掉的數字。沒有數字就不畫這顆；repo 名與 tooltip 已經說明這是什麼。 */}
        {openCount === null ? null : (
          <span className="issues-count">{openCount >= 100 ? '100+' : openCount} open</span>
        )}
      </button>
      {open ? (
        <div className="issues-pop" role="dialog" aria-label="GitHub issues">
          <div className="issues-tools">
            <input
              ref={searchRef}
              type="search"
              value={q}
              placeholder="搜尋 issue（標題 / #號）…"
              aria-label="搜尋 issue（標題或 #號）"
              spellCheck={false}
              onChange={(e) => setQ(e.target.value)}
            />
            {submodules.length > 0 ? (
              <select
                className="issues-repo-pick"
                aria-label="哪個 repo 的 issue"
                title="這個專案有 submodule：選要看哪個 repo 的 issue（組隊也會在那個 repo 裡進行）"
                value={repo}
                onChange={(e) => {
                  setRepo(e.target.value)
                  setIssues(null)
                }}
              >
                <option value="">{github.owner}/{github.repo}</option>
                {submodules.map((s) => (
                  <option key={s.path} value={s.path}>
                    {s.path} · {s.github?.owner}/{s.github?.repo}
                  </option>
                ))}
              </select>
            ) : null}
            <div className="tabs small" role="tablist" aria-label="issue 狀態" onKeyDown={onTabListKeyDown}>
              <button type="button" className="tab" role="tab" id={`${tabsId}-open`} aria-selected={state === 'open'} aria-controls={state === 'open' ? `${tabsId}-panel` : undefined} onClick={() => pickState('open')}>
                open
              </button>
              <button type="button" className="tab" role="tab" id={`${tabsId}-closed`} aria-selected={state === 'closed'} aria-controls={state === 'closed' ? `${tabsId}-panel` : undefined} onClick={() => pickState('closed')}>
                closed
              </button>
            </div>
          </div>
          {/* 結果清單就是那兩個分頁的 tabpanel（同一塊，內容跟著 open / closed 換）。 */}
          <div role="tabpanel" id={`${tabsId}-panel`} aria-labelledby={`${tabsId}-${state}`}>
          {error ? (
            <div className="issues-status err" role="alert">
              <div>{error}</div>
              {authError ? <GhLoginButton host={host} onLoggedIn={retryIssues} /> : null}
            </div>
          ) : loading && !issues ? (
            <div className="issues-status">載入中…</div>
          ) : issues && issues.length === 0 ? (
            <div className="issues-status">沒有符合的 {state} issue{q ? `（${q}）` : ''}。</div>
          ) : null}
          {issues && issues.length > 0 ? (
            <ul className={`issues-list${loading ? ' stale' : ''}`}>
              {issues.map((i) => (
                <li key={i.number} className="issue-row">
                  <div className="issue-main">
                    <a className="issue-title" href={i.url} target="_blank" rel="noreferrer" title={i.body_excerpt || i.title}>
                      <span className="issue-num">#{i.number}</span> {i.title}
                    </a>
                    <div className="issue-meta">
                      {i.labels.map((l) => (
                        <span key={l.name} className="issue-label" style={labelStyle(l.color)}>
                          {l.name}
                        </span>
                      ))}
                      {i.author ? <span className="issue-author">{i.author}</span> : null}
                      <span className="issue-time" title={i.updated_at}>
                        {ago(i.updated_at)}
                      </span>
                    </div>
                  </div>
                  <div className="issue-actions">
                    {teamsSupported ? (
                      <button
                        type="button"
                        className="mini-btn team-btn"
                        title="為這個 issue 建立一個 team（PM + 執行者 + reviewer，各自獨立的 worktree）"
                        onClick={() => {
                          openTeamLaunch(projectId, i.number, repo)
                          setOpen(false)
                        }}
                      >
                        組隊
                      </button>
                    ) : null}
                    <button type="button" className="mini-btn" title="插入「#號 標題」與連結到輸入框游標處" onClick={() => insertRef(i)}>
                      插入
                    </button>
                    <button
                      type="button"
                      className="mini-btn primary"
                      disabled={fetching === i.number}
                      title="再讀取完整內容，以引用區塊（> ）插入"
                      onClick={() => void insertFull(i)}
                    >
                      {fetching === i.number ? '讀取中…' : '插入完整內容'}
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          ) : null}
          </div>
        </div>
      ) : null}
    </div>
  )
}
