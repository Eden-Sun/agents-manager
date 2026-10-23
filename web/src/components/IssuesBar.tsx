import { useCallback, useEffect, useId, useRef, useState } from 'react'
import type { RefObject } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import type { Issue, ProjectSubmodule } from '../api/types'
import { useStore } from '../store/store'
import type { DraftKey } from '../store/store'
import { labelStyle } from '../lib/labelStyle'
import { formatGithubIssuePrompt } from '../lib/githubIssuePrompt'
import { GhLoginButton } from './GhAuth'
import { isGhAuthError } from './ghAuthError'
import { ISSUE_PAGE_LIMIT, openCountFromPage } from '../lib/issueCount'
import { onTabListKeyDown } from './tabKeys'
import './issuesBar.css'
import { safeHttpUrl } from '../lib/safeUrl'
import { agoLabel } from '../lib/agoLabel'
import { t, useT } from '../lib/i18n'

/** v4.0 GitHub issues (via `gh` on the daemon): search, open / closed toggle, insert into the composer. */

const STATE_KEY = 'am.issueState'
type IssueState = 'open' | 'closed'
type IssueScope = { projectId: string; githubUrl: string; repo: string; state: IssueState; query: string; generation: number }

function sameIssueScope(a: IssueScope, b: IssueScope): boolean {
  return a.projectId === b.projectId && a.githubUrl === b.githubUrl && a.repo === b.repo && a.state === b.state && a.query === b.query && a.generation === b.generation
}

function readState(): IssueState {
  try {
    return localStorage.getItem(STATE_KEY) === 'closed' ? 'closed' : 'open'
  } catch {
    return 'open'
  }
}

function errorText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 502) return t('gh 無法使用：{message}', { message: e.message })
    return t('{message}（HTTP {status}）', { message: e.message, status: e.status })
  }
  return e instanceof Error ? e.message : String(e)
}

export function IssuesBar({
  projectId,
  draftKey,
  inputRef,
  onInserted,
}: {
  projectId: string
  draftKey: DraftKey
  inputRef: RefObject<HTMLTextAreaElement | null>
  /** 插完之後由外層處理（手機：彈窗收掉）。有給就不把焦點搶回輸入框（#1241）。 */
  onInserted?: () => void
}) {
  const github = useStore((s) => s.projects.find((p) => p.id === projectId)?.github ?? null)
  // deps 用字串：`github` 物件每次 normalize 都重建，當 deps 會讓任何 state 事件都重抓（快取冷了就是一次 gh）。
  const githubUrl = github?.url ?? null
  const host = useStore((s) => s.projects.find((p) => p.id === projectId)?.host ?? 'local')
  const setDraft = useStore((s) => s.setDraft)
  const setDraftCursor = useStore((s) => s.setDraftCursor)
  const notify = useStore((s) => s.notify)
  const [open, setOpen] = useState(false)
  const [state, setState] = useState<IssueState>(readState)
  const [q, setQ] = useState('')
  const [issues, setIssues] = useState<Issue[] | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [authError, setAuthError] = useState(false)
  const [openCount, setOpenCount] = useState<number | null>(null)
  const [fetching, setFetching] = useState<number | null>(null)
  const [issuesScope, setIssuesScope] = useState<IssueScope | null>(null)
  // `''` = 專案本身；掛在 projectId 上，換專案自然歸零，不用在 effect 裡 setState。
  const [subsFor, setSubsFor] = useState<{ pid: string; subs: ProjectSubmodule[] }>({ pid: '', subs: [] })
  const [repoFor, setRepoFor] = useState<{ pid: string; repo: string }>({ pid: '', repo: '' })
  const submodules = subsFor.pid === projectId ? subsFor.subs : []
  const repo = repoFor.pid === projectId ? repoFor.repo : ''
  const setRepo = (r: string) => setRepoFor({ pid: projectId, repo: r })
  const wrap = useRef<HTMLDivElement>(null)
  const searchRef = useRef<HTMLInputElement>(null)
  const seq = useRef(0)
  const scopeGen = useRef(0)
  const detailSeq = useRef(0)
  const tabsId = useId()
  const t = useT()

  useEffect(() => {
    if (!githubUrl) return
    let alive = true
    api
      .fetchSubmodules(projectId)
      .then((subs) => alive && setSubsFor({ pid: projectId, subs: subs.filter((s) => s.github) }))
      .catch(() => alive && setSubsFor({ pid: projectId, subs: [] }))
    return () => {
      alive = false
    }
  }, [projectId, githubUrl])

  useEffect(() => {
    if (!githubUrl) return
    let alive = true
    api
      .fetchIssues(projectId, { state: 'open', limit: 100, repo })
      .then((list) => alive && setOpenCount(list.length))
      .catch(() => alive && setOpenCount(null))
    return () => {
      alive = false
    }
  }, [projectId, githubUrl, repo])

  useEffect(() => {
    scopeGen.current++
    seq.current++
    detailSeq.current++
    setIssues(null)
    setIssuesScope(null)
    setLoading(false)
    setFetching(null)
    setError(null)
    setAuthError(false)
  }, [projectId, githubUrl])

  const invalidateScope = () => {
    scopeGen.current++
    seq.current++
    detailSeq.current++
    setLoading(false)
    setFetching(null)
    setError(null)
    setAuthError(false)
  }

  const load = useCallback(
    async (st: IssueState, query: string) => {
      const id = ++seq.current
      const generation = scopeGen.current
      const requestScope: IssueScope = { projectId, githubUrl: githubUrl ?? '', repo, state: st, query: query.trim(), generation }
      setLoading(true)
      setError(null)
      setAuthError(false)
      try {
        const list = await api.fetchIssues(projectId, { state: st, limit: ISSUE_PAGE_LIMIT, q: query.trim() || undefined, repo })
        if (id !== seq.current || generation !== scopeGen.current) return
        setIssues(list)
        setIssuesScope(requestScope)
        if (st === 'open' && !query.trim()) setOpenCount((prev) => openCountFromPage(prev, list.length))
      } catch (e) {
        if (id !== seq.current || generation !== scopeGen.current) return
        setIssues(null)
        setIssuesScope(null)
        setAuthError(isGhAuthError(e))
        setError(errorText(e))
      } finally {
        if (id === seq.current && generation === scopeGen.current) setLoading(false)
      }
    },
    [projectId, githubUrl, repo],
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

  const currentScope: IssueScope = { projectId, githubUrl: githubUrl ?? '', repo, state, query: q.trim(), generation: scopeGen.current }
  const shownIssues = issuesScope && sameIssueScope(issuesScope, currentScope) ? issues : null
  const shown = submodules.find((s) => s.path === repo)?.github ?? github

  const pickState = (st: IssueState) => {
    invalidateScope()
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
    setOpen(false)
    if (onInserted) {
      // 手機：這顆 IssuesBar 在彈窗裡，輸入框在彈窗外面；焦點 focus 過去會被焦點陷阱拉回來，看起來像沒反應（#1241）。
      // 不自動叫出鍵盤，同 Composer 的 `autoFocus: !phone`。
      onInserted()
      return
    }
    requestAnimationFrame(() => {
      const ta = inputRef.current
      if (!ta) return
      ta.focus()
      ta.setSelectionRange(pos, pos)
    })
  }

  const insertRef = (i: Issue, source: IssueScope) => {
    if (source.generation !== scopeGen.current) return
    insert(formatGithubIssuePrompt(i))
  }

  const insertFull = async (i: Issue, source: IssueScope) => {
    if (source.generation !== scopeGen.current) return
    const detailId = ++detailSeq.current
    setFetching(i.number)
    try {
      const d = await api.fetchIssue(source.projectId, i.number, source.repo)
      if (source.generation !== scopeGen.current) return
      const body = (d?.body ?? '').trim() || t('（沒有內容）')
      insert(formatGithubIssuePrompt({ ...i, content_notice: d?.content_notice ?? i.content_notice }, body))
    } catch (e) {
      if (source.generation !== scopeGen.current) return
      notify('error', t('讀取 #{number} 失敗：{reason}', { number: i.number, reason: errorText(e) }))
    } finally {
      if (detailId === detailSeq.current) setFetching(null)
    }
  }

  return (
    <div className="issues-bar" ref={wrap}>
      <button
        type="button"
        className={`issues-btn${open ? ' on' : ''}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        title={t('{url}/issues — 點開搜尋並插入 issue 到輸入框', { url: github.url })}
        onClick={() => setOpen(!open)}
      >
        <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
          <circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" strokeWidth="1.5" />
          <circle cx="8" cy="8" r="2" fill="currentColor" />
        </svg>
        <span className="issues-repo">
          <span className="issues-owner">{shown.owner}/</span>
          {shown.repo}
        </span>
        {/* 沒有數字就不畫：藥丸裡放單字讀起來像壞掉的數字。 */}
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
              placeholder={t('搜尋 issue（標題 / #號）…')}
              aria-label={t('搜尋 issue（標題或 #號）')}
              spellCheck={false}
              onChange={(e) => {
                invalidateScope()
                setQ(e.target.value)
              }}
            />
            {submodules.length > 0 ? (
              <select
                className="issues-repo-pick"
                aria-label={t('哪個 repo 的 issue')}
                title={t('這個專案有 submodule：選要看哪個 repo 的 issue')}
                value={repo}
                onChange={(e) => {
                  invalidateScope()
                  setRepo(e.target.value)
                  setIssues(null)
                  setIssuesScope(null)
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
            <div className="tabs small" role="tablist" aria-label={t('issue 狀態')} onKeyDown={onTabListKeyDown}>
              <button type="button" className="tab" role="tab" id={`${tabsId}-open`} aria-selected={state === 'open'} aria-controls={state === 'open' ? `${tabsId}-panel` : undefined} onClick={() => pickState('open')}>
                open
              </button>
              <button type="button" className="tab" role="tab" id={`${tabsId}-closed`} aria-selected={state === 'closed'} aria-controls={state === 'closed' ? `${tabsId}-panel` : undefined} onClick={() => pickState('closed')}>
                closed
              </button>
            </div>
          </div>
          <div role="tabpanel" id={`${tabsId}-panel`} aria-labelledby={`${tabsId}-${state}`}>
          {error ? (
            <div className="issues-status err" role="alert">
              <div>{error}</div>
              {authError ? <GhLoginButton host={host} onLoggedIn={retryIssues} /> : null}
            </div>
          ) : loading && !shownIssues ? (
            <div className="issues-status">{t('載入中…')}</div>
          ) : shownIssues && shownIssues.length === 0 ? (
            <div className="issues-status">
              {q
                ? t('沒有符合的 {state} issue（{q}）。', { state, q })
                : t('沒有符合的 {state} issue。', { state })}
            </div>
          ) : null}
          {shownIssues && shownIssues.length > 0 && issuesScope ? (
            <ul className={`issues-list${loading ? ' stale' : ''}`}>
              {shownIssues.map((i) => (
                <li key={i.number} className="issue-row">
                  <div className="issue-main">
                    <a className="issue-title" href={safeHttpUrl(i.url)} target="_blank" rel="noopener noreferrer" title={i.body_excerpt || i.title}>
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
                        {agoLabel(i.updated_at)}
                      </span>
                    </div>
                  </div>
                  <div className="issue-actions">
                    <button type="button" className="mini-btn" title={t('插入「#號 標題」與連結到輸入框游標處')} onClick={() => insertRef(i, issuesScope)}>
                      {t('插入')}
                    </button>
                    <button
                      type="button"
                      className="mini-btn primary"
                      disabled={fetching === i.number}
                      title={t('再讀取完整內容，以引用區塊（> ）插入')}
                      onClick={() => void insertFull(i, issuesScope)}
                    >
                      {fetching === i.number ? t('讀取中…') : t('插入完整內容')}
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
