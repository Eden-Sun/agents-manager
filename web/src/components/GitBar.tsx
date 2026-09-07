import { useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../api'
import type { GitSummary } from '../api'
import { ApiError } from '../api/types'
import { useStore } from '../store/store'

/**
 * 專案 checkout 的 git 一眼看（2026-09-08）：`+N −M`（工作樹的行數差）、`↑a ↓b`（跟 upstream
 * 的 commit 差）、以及三顆快捷鍵：commit（`git add -A && git commit`）、push、pull
 * （`--rebase --no-autostash`）。放在 chat 標題列的 repo chip 旁邊，不是 git 客戶端——只是
 * 「agent 剛改完，我要立刻推出去」這一個手勢。
 *
 * 不是 git repo（或舊 daemon 沒這支端點）整條消失。每 15 秒重讀一次，做完動作立刻重讀。
 */
const POLL_MS = 15_000

function errText(e: unknown): string {
  if (e instanceof ApiError) {
    const out = typeof e.body.output === 'string' ? e.body.output.trim() : ''
    if (e.body.reason === 'nothing_to_commit') return '沒有可以 commit 的變更'
    return out || e.message
  }
  return e instanceof Error ? e.message : String(e)
}

export function GitBar({ projectId }: { projectId: string }) {
  const notify = useStore((s) => s.notify)
  const [sum, setSum] = useState<GitSummary | null>(null)
  const [busy, setBusy] = useState<'commit' | 'push' | 'pull' | null>(null)
  const [composing, setComposing] = useState(false)
  const [msg, setMsg] = useState('')
  const inputRef = useRef<HTMLInputElement>(null)

  const refresh = useCallback(async () => {
    try {
      setSum(await api.fetchGit(projectId))
    } catch {
      // 讀不到就維持上一次的值；下一輪再試。
    }
  }, [projectId])

  useEffect(() => {
    let alive = true
    let timer: ReturnType<typeof setTimeout> | null = null
    const tick = async () => {
      await refresh()
      if (alive) timer = setTimeout(() => void tick(), POLL_MS)
    }
    void tick()
    return () => {
      alive = false
      if (timer) clearTimeout(timer)
    }
  }, [refresh])

  useEffect(() => {
    if (composing) inputRef.current?.focus()
  }, [composing])

  if (!sum?.git) return null

  const dirty = sum.changed + sum.untracked > 0

  async function run(op: 'commit' | 'push' | 'pull') {
    if (busy) return
    setBusy(op)
    try {
      const out = await api.gitAction(projectId, op, op === 'commit' ? msg : undefined)
      if (op === 'commit') {
        setMsg('')
        setComposing(false)
      }
      const label = op === 'commit' ? '已 commit' : op === 'push' ? '已 push' : '已 pull'
      notify('info', out ? `${label}：${out.split('\n').slice(-1)[0]}` : label)
    } catch (e) {
      notify('error', `git ${op} 失敗：${errText(e)}`)
    } finally {
      setBusy(null)
      void refresh()
    }
  }

  const title = [
    sum.branch ? `分支 ${sum.branch}` : '分離的 HEAD',
    sum.upstream ? `upstream ${sum.upstream}` : '沒有 upstream（push 會自動 -u origin）',
    `${sum.changed} 個檔案有改動、${sum.untracked} 個未追蹤`,
  ].join('\n')

  return (
    <div className="git-bar" title={title}>
      <span className={`git-stat${dirty ? ' dirty' : ''}`}>
        <span className="git-ins">+{sum.insertions}</span>
        <span className="git-del">−{sum.deletions}</span>
        {sum.untracked ? <span className="git-untracked">?{sum.untracked}</span> : null}
      </span>
      {sum.ahead || sum.behind ? (
        <span className="git-ab">
          {sum.ahead ? <span title={`比 upstream 多 ${sum.ahead} 個 commit`}>↑{sum.ahead}</span> : null}
          {sum.behind ? <span title={`比 upstream 少 ${sum.behind} 個 commit`}>↓{sum.behind}</span> : null}
        </span>
      ) : null}
      {composing ? (
        <form
          className="git-commit-form"
          onSubmit={(e) => {
            e.preventDefault()
            if (msg.trim()) void run('commit')
          }}
        >
          <input
            ref={inputRef}
            type="text"
            className="git-commit-msg"
            placeholder="commit 訊息，Enter 送出"
            value={msg}
            onChange={(e) => setMsg(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') setComposing(false)
            }}
            disabled={busy === 'commit'}
          />
          <button type="submit" className="mini-btn" disabled={!msg.trim() || busy === 'commit'}>
            {busy === 'commit' ? '送出中…' : 'commit'}
          </button>
          <button type="button" className="mini-btn" onClick={() => setComposing(false)}>
            取消
          </button>
        </form>
      ) : (
        <>
          <button
            type="button"
            className="mini-btn"
            disabled={!dirty || busy !== null}
            title={dirty ? 'git add -A && git commit' : '工作樹是乾淨的'}
            onClick={() => setComposing(true)}
          >
            commit
          </button>
          <button
            type="button"
            className="mini-btn"
            disabled={busy !== null || (!sum.ahead && Boolean(sum.upstream))}
            title={sum.upstream ? 'git push' : 'git push -u origin HEAD'}
            onClick={() => void run('push')}
          >
            {busy === 'push' ? 'push…' : 'push'}
          </button>
          <button
            type="button"
            className="mini-btn"
            disabled={busy !== null}
            title="git pull --rebase --no-autostash"
            onClick={() => void run('pull')}
          >
            {busy === 'pull' ? 'pull…' : 'pull'}
          </button>
        </>
      )}
    </div>
  )
}
