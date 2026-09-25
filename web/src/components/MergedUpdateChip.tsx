import { useEffect, useRef, useState, type CSSProperties } from 'react'
import { createPortal } from 'react-dom'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { codexInstallPlan, updateBatchCounts } from '../lib/updateBatch'
import { updateRange } from '../lib/updateRange'
import { mergedUpdateLabels } from '../lib/mergedUpdateLabels'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { UpgradeIcon } from './UpgradeIcon'
import { CodexInstallChip } from './CodexInstallChip'
import { RestartChip } from './UpdateQuotaChip'

/** 由外面代為開關的確認框（手機合成那顆用）。 */
export interface DialogControl {
  open: boolean
  close: () => void
}

/**
 * 手機（≤640px）上「重啟套用」與「codex 安裝＋重啟」兩顆都要出現時合成的一顆 `⌃⌃ N`（UI-DECISIONS「header 的 codex 安裝」）。
 * 名字行在 390px 很擠（長名字實測：沒有 chip 名字 84px、一顆 44px、兩顆只剩 16px＝只看得到 ▾）；合成這顆只留圖示，名字 66px。
 * 點開是兩項的小選單，各自打開原本那個確認框——兩件事的代價不同，確認框照舊分開。
 */
export function MergedUpdateChip() {
  const batch = useStore((s) => s.restartBatch)
  const clear = useStore((s) => s.clearRestartBatch)
  const cli = useStore((s) => s.cliUpdate)
  const readyCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.length)
  const busyCount = useStore(
    (s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).busy.filter((b) => !b.install).length,
  )
  const installCount = useStore(
    (s) =>
      codexInstallPlan(
        s.bots,
        s.runs,
        (id) => inFlightTurn(s, id) !== null,
        (b) => projectHostName(s, b.project_id),
      )?.installCount ?? 0,
  )
  const codexNotice = useStore(
    (s) =>
      codexInstallPlan(
        s.bots,
        s.runs,
        (id) => inFlightTurn(s, id) !== null,
        (b) => projectHostName(s, b.project_id),
      )?.notice ?? '',
  )
  const [open, setOpen] = useState(false)
  // 標題列一路有 overflow 裁切與 transform（fixed 也會被帶偏），選單用 portal 掛到 body、fixed 在按鈕下緣。
  const [at, setAt] = useState<CSSProperties>({})
  const [which, setWhich] = useState<'restart' | 'codex' | null>(null)
  const btn = useRef<HTMLButtonElement>(null)
  const pop = useRef<HTMLDivElement>(null)
  const menuKeys = useMenuKeys(open, pop, btn, () => setOpen(false))

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      const t = e.target as Node
      if (!btn.current?.contains(t) && !pop.current?.contains(t)) setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
  }, [open])

  const { restartItem, codexItem, label } = mergedUpdateLabels({
    batch,
    cli,
    readyCount,
    busyCount,
    installCount,
    to: updateRange('codex', codexNotice, null).to,
  })
  const close = () => setWhich(null)

  return (
    <>
      <button
        ref={btn}
        type="button"
        className={`quota-update install merged${open ? ' on' : ''}`}
        aria-haspopup="menu"
        aria-expanded={open}
        title={label}
        aria-label={label}
        onClick={(e) => {
          const r = e.currentTarget.getBoundingClientRect()
          // 只在手機出現：貼螢幕右側 8px，最寬 300px，不會滑出左緣。
          setAt({ position: 'fixed', top: Math.round(r.bottom + 6), right: 8 })
          setOpen((v) => !v)
        }}
      >
        <span aria-hidden="true">
          <UpgradeIcon />
        </span>
        {/* 手機只留圖示：數字寫在 tooltip／選單裡，省下的寬度給名字。 */}
      </button>
      {open
        ? createPortal(
            <div
              ref={pop}
              className="head-menu-pop update-menu-pop"
              style={at}
              role="menu"
              aria-label="更新"
              onClick={() => setOpen(false)}
              onKeyDown={menuKeys}
            >
              <button
                type="button"
                className="head-menu-item"
                role="menuitem"
                tabIndex={-1}
                disabled={!batch && readyCount === 0}
                onClick={() => (batch ? clear() : setWhich('restart'))}
              >
                {restartItem}
              </button>
              <button
                type="button"
                className="head-menu-item"
                role="menuitem"
                tabIndex={-1}
                disabled={Boolean(cli)}
                onClick={() => setWhich('codex')}
              >
                {codexItem}
              </button>
            </div>,
            document.body,
          )
        : null}
      <RestartChip control={{ open: which === 'restart', close }} />
      <CodexInstallChip control={{ open: which === 'codex', close }} />
    </>
  )
}
