import { useT } from "../lib/i18n"
import { copyText } from '../lib/copyText'
import { useEffect, useRef, useState } from 'react'
import { TerminalIcon } from './Icons'
import './attachButton.css'

/** v4.0 "open in terminal": one-click copy of the host's `herdr …` attach command; a popover shows what was copied. */
export function AttachButton({ command, compact }: { command: string; compact?: boolean }) {
  const t = useT()
  const [open, setOpen] = useState(false)
  const [copied, setCopied] = useState<'ok' | 'fail' | null>(null)
  /** 最近一次複製的結果：說明文字看這個（`copied` 1.6 秒後會被清掉，不能拿來講「已複製」）。 */
  const [lastCopy, setLastCopy] = useState<'ok' | 'fail' | null>(null)
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
    const r = (await copyText(command)) ? 'ok' : 'fail'
    setCopied(r)
    setLastCopy(r)
    setTimeout(() => setCopied(null), 1600)
    return r
  }

  const onMainClick = () => {
    if (open) {
      setOpen(false)
      return
    }
    void copy().then(() => setOpen(true))
  }

  if (!command) return null

  return (
    <span className={`attach${compact ? ' compact' : ''}`} ref={wrap} onClick={(e) => e.stopPropagation()}>
      <button
        type="button"
        className={`icon-btn attach-btn icon-tip${open ? ' on' : ''}${copied === 'ok' ? ' copied' : ''}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={open ? t('關閉 attach 指令') : t('複製 attach 指令')}
        data-tip={open ? t('關閉 attach 指令 · 終端') : t('複製 attach 指令 · 終端')}
        onClick={onMainClick}
      >
        {open ? (
          <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
            <path d="M3.5 3.5l9 9M12.5 3.5l-9 9" stroke="currentColor" strokeWidth="1.7" fill="none" strokeLinecap="round" />
          </svg>
        ) : (
          <TerminalIcon />
        )}
        {compact ? null : <span className="attach-label">{copied === 'ok' ? t('已複製') : t('在終端開啟')}</span>}
      </button>
      {open ? (
        <div className="attach-pop" role="dialog" aria-label={t('attach 指令')}>
          <div className="attach-hint">
            {lastCopy === 'fail'
              ? t('沒能複製到剪貼簿（這個瀏覽器不讓網頁寫入）；請手動選取下方的指令再複製，在本機終端貼上即可看到同一個 herdr session。')
              : t('已複製到剪貼簿；在本機終端貼上即可看到同一個 herdr session：')}
          </div>
          <div className="attach-row">
            <code className="attach-cmd">{command}</code>
            <button type="button" className={`btn primary${copied === 'ok' ? ' ok' : ''}`} onClick={() => void copy()}>
              {copied === 'ok' ? t('已複製 ✓') : copied === 'fail' ? t('複製失敗') : t('再複製')}
            </button>
          </div>
        </div>
      ) : null}
    </span>
  )
}
