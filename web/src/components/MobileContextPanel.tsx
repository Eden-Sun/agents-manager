import { useLayoutEffect, useRef } from 'react'
import type { ReactNode, RefObject } from 'react'
import { createPortal } from 'react-dom'
import type { StatusInfo } from '../api/types'
import { useCacheInput } from '../hooks/useCacheInput'
import { cacheView } from '../lib/composerCost'
import { useT } from '../lib/i18n'
import { CompactButton } from './CompactButton'
import { GitBar } from './GitBar'
import { KeepWarmSkipButton } from './KeepWarmSkipButton'
import './mobileContextPanel.css'

/** 手機 context 入口的小 popup；portal 避開標題列與對話容器的裁切。 */
export function MobileContextPanel({ botId, projectId, status, reading, issues, anchor, onClose }: {
  botId: string
  projectId: string
  status: StatusInfo | null
  reading: { pct: number | null; detail: string | null; title?: string; unknown: boolean }
  issues: ReactNode
  anchor: RefObject<HTMLButtonElement | null>
  onClose: () => void
}) {
  const t = useT()
  const panel = useRef<HTMLDivElement>(null)
  const closeRef = useRef(onClose)
  useLayoutEffect(() => { closeRef.current = onClose })
  useLayoutEffect(() => {
    const root = panel.current
    const button = anchor.current
    if (!root || !button) return
    const position = () => {
      const rect = button.getBoundingClientRect()
      const width = Math.min(300, window.innerWidth - 16)
      root.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - width - 8))}px`
      root.style.top = `${rect.bottom + 6}px`
      root.style.maxHeight = `${Math.max(80, window.innerHeight - rect.bottom - 14)}px`
    }
    const outside = (event: PointerEvent) => {
      if (event.target instanceof Node && !root.contains(event.target) && !button.contains(event.target)) closeRef.current()
    }
    const escape = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && !event.isComposing) {
        event.preventDefault()
        closeRef.current()
      }
    }
    position()
    root.focus()
    document.addEventListener('pointerdown', outside)
    document.addEventListener('keydown', escape)
    window.addEventListener('resize', position)
    window.addEventListener('scroll', position, true)
    return () => {
      document.removeEventListener('pointerdown', outside)
      document.removeEventListener('keydown', escape)
      window.removeEventListener('resize', position)
      window.removeEventListener('scroll', position, true)
      if (root.contains(document.activeElement) || document.activeElement === document.body) button.focus()
    }
  }, [anchor])
  const cacheInput = useCacheInput(botId)
  const cache = cacheView(cacheInput)
  const cacheMinutes = cache?.mins ?? (cache?.state === 'hot' ? cacheView({ ...cacheInput, working: false })?.mins : null)
  const percentage = reading.unknown ? null : reading.pct
  const detail = reading.unknown ? null : reading.detail
  return createPortal(
    <div className="mobile-context-panel" role="dialog" aria-label={t('Context 與專案')} tabIndex={-1} ref={panel}>
      <div className="mobile-context-reading" title={reading.unknown ? undefined : reading.title}>
        {detail ? detail.replace('/', ' / ') : '—'}{percentage !== null ? ` · ${Math.round(percentage)}%` : ''}
      </div>
      <progress aria-label={t('Context 已用比例')} max={100} value={percentage === null ? 0 : Math.min(100, Math.max(0, percentage))} aria-valuetext={percentage === null ? '—' : `${Math.round(percentage)}%`} />
      <div className="mobile-context-cache">{cacheInput.kind === 'codex' ? t('必快取') : cache ? cache.state === 'cold' ? t('已冷') : cacheMinutes != null ? t('熱 · 剩 {n} 分', { n: cacheMinutes }) : t('熱') : '—'}</div>
      {/* 壓縮與「不用保溫」並排（2026-11-07 使用者：popup 也要能設定不保溫；只有主力的 claude 會畫出後者）。 */}
      <div className="mobile-context-actions">
        <CompactButton botId={botId} />
        <KeepWarmSkipButton botId={botId} />
      </div>
      {status?.account_warning ? <div className="mobile-context-warning sl-warn" title={status.account_warning}>{t('⚠ 未登入，用的是預設帳號')}</div> : null}
      <GitBar projectId={projectId} showBranch showActions={false} />
      {issues}
    </div>, document.body,
  )
}
