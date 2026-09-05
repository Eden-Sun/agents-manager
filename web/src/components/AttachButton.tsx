import { useEffect, useRef, useState } from 'react'

/**
 * v4.0 "open in terminal": one-click copy of the host's `herdr …` attach command
 * (`navigator.clipboard`). A short popover still shows the command so the user can
 * confirm what was copied. Running it in a local terminal attaches to the same
 * herdr session the daemon drives.
 */
export function AttachButton({ command, compact }: { command: string; compact?: boolean }) {
  const [open, setOpen] = useState(false)
  const [copied, setCopied] = useState<'ok' | 'fail' | null>(null)
  const wrap = useRef<HTMLSpanElement>(null)

  useEffect(() => {
    if (!open) return
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

  const copy = async (): Promise<'ok' | 'fail'> => {
    try {
      await navigator.clipboard.writeText(command)
      setCopied('ok')
      setTimeout(() => setCopied(null), 1600)
      return 'ok'
    } catch {
      setCopied('fail')
      setTimeout(() => setCopied(null), 1600)
      return 'fail'
    }
  }

  const onMainClick = () => {
    // One-click copy; keep the popover open so the command stays visible.
    void copy().then(() => setOpen(true))
  }

  if (!command) return null

  return (
    <span className={`attach${compact ? ' compact' : ''}`} ref={wrap} onClick={(e) => e.stopPropagation()}>
      <button
        type="button"
        className={`icon-btn attach-btn${open ? ' on' : ''}${copied === 'ok' ? ' copied' : ''}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        title={`複製 attach 指令：${command}`}
        aria-label="複製 attach 指令"
        onClick={onMainClick}
      >
        <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
          <rect x="1.5" y="2.5" width="13" height="11" rx="2" fill="none" stroke="currentColor" strokeWidth="1.5" />
          <path d="M4.5 6l2.5 2-2.5 2M8.5 10.5h3" stroke="currentColor" strokeWidth="1.5" fill="none" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
        {compact ? null : <span className="attach-label">{copied === 'ok' ? '已複製' : '在終端開啟'}</span>}
      </button>
      {open ? (
        <div className="attach-pop" role="dialog" aria-label="attach 指令">
          <div className="attach-hint">已複製到剪貼簿；在本機終端貼上即可看到同一個 herdr session：</div>
          <div className="attach-row">
            <code className="attach-cmd">{command}</code>
            <button type="button" className={`btn primary${copied === 'ok' ? ' ok' : ''}`} onClick={() => void copy()}>
              {copied === 'ok' ? '已複製 ✓' : copied === 'fail' ? '複製失敗' : '再複製'}
            </button>
          </div>
        </div>
      ) : null}
    </span>
  )
}
