import { useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../api'
import type { GitSummary } from '../api'
import { ApiError } from '../api/types'
import { useStore } from '../store/store'
import './gitBar.css'
import { t, useT } from '../lib/i18n'

/**
 * 專案 checkout 的 git 一眼看（2026-09-08）：行數差、upstream 差，與 commit / push / pull 快捷鍵。
 * 不是 git client，只服務「agent 剛改完，立刻推出去」這個手勢。
 */
const POLL_MS = 15_000

function errText(e: unknown): string {
  if (e instanceof ApiError) {
    const out = typeof e.body.output === 'string' ? e.body.output.trim() : ''
    if (e.body.reason === 'nothing_to_commit') return t('沒有可以 commit 的變更')
    return out || e.message
  }
  return e instanceof Error ? e.message : String(e)
}

export function GitBar({ projectId }: { projectId: string }) {
  const t = useT()
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
    const visible = () => document.visibilityState === 'visible'
    const tick = async () => {
      // 背景不抓；timer 照排，回前景後自然接上。
      if (visible()) await refresh()
      if (alive) timer = setTimeout(() => void tick(), POLL_MS)
    }
    // 回前景立刻補一次，不等下一個 tick。
    const onVisible = () => {
      if (visible()) void refresh()
    }
    void tick()
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      alive = false
      if (timer) clearTimeout(timer)
      document.removeEventListener('visibilitychange', onVisible)
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
      const label = op === 'commit' ? t('已 commit') : op === 'push' ? t('已 push') : t('已 pull')
      notify('info', out ? t('{label}：{out}', { label, out: out.split('\n').slice(-1)[0] }) : label)
    } catch (e) {
      notify('error', t('git {op} 失敗：{reason}', { op, reason: errText(e) }))
    } finally {
      setBusy(null)
      void refresh()
    }
  }

  const title = [
    sum.branch ? t('分支 {name}', { name: sum.branch }) : t('分離的 HEAD'),
    sum.upstream ? t('upstream {name}', { name: sum.upstream }) : t('沒有 upstream（push 會自動 -u origin）'),
    t('{changed} 個檔案有改動、{untracked} 個未追蹤', { changed: sum.changed, untracked: sum.untracked }),
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
          {sum.ahead ? <span title={t('比 upstream 多 {n} 個 commit', { n: sum.ahead })}>↑{sum.ahead}</span> : null}
          {sum.behind ? <span title={t('比 upstream 少 {n} 個 commit', { n: sum.behind })}>↓{sum.behind}</span> : null}
        </span>
      ) : null}
      {/* 桌面 hover 才彈出（2026-09-12 使用者）；打 commit 訊息時 `.open` 常駐。 */}
      <span className={`git-actions${composing ? ' open' : ''}`}>
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
            placeholder={t('commit 訊息，Enter 送出')}
            value={msg}
            onChange={(e) => setMsg(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') setComposing(false)
            }}
            disabled={busy === 'commit'}
          />
          <button type="submit" className="mini-btn" disabled={!msg.trim() || busy === 'commit'}>
            {busy === 'commit' ? t('送出中…') : 'commit'}
          </button>
          <button type="button" className="mini-btn" onClick={() => setComposing(false)}>
            {t('取消')}
          </button>
        </form>
      ) : (
        <>
          <button
            type="button"
            className="mini-btn"
            disabled={!dirty || busy !== null}
            title={dirty ? 'git add -A && git commit' : t('工作樹是乾淨的')}
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
      </span>
    </div>
  )
}
