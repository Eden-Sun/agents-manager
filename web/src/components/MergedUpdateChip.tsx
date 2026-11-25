import { useEffect, useRef, useState, type CSSProperties } from 'react'
import { createPortal } from 'react-dom'
import { inFlightTurn, useStore } from '../store/store'
import { updateBatchCounts } from '../lib/updateBatch'
import { useInstallTodo } from '../hooks/useInstallTodo'
import { mergedUpdateLabels } from '../lib/mergedUpdateLabels'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { useUpdateChipRequest } from '../hooks/useUpdateChipRequest'
import { UpgradeIcon } from './UpgradeIcon'
import { RestartChip } from './UpdateQuotaChip'
import { HerdrUpdateChip } from './HerdrUpdateChip'
import { GrokUpdateChip } from './GrokUpdateChip'
import { AgyUpdateChip } from './AgyUpdateChip'
import { useHerdrPlan } from '../hooks/useHerdrPlan'
import { useGrokPlan } from '../hooks/useGrokPlan'
import { useAgyBusy, useAgyPlan } from '../hooks/useAgyPlan'
import { grokMenuItem } from '../lib/grokUpdate'
import { agyMenuItem } from '../lib/agyUpdate'
import { herdrMenuItem, useHerdrUpdate } from '../store/herdrUpdate'
import { useT } from '../lib/i18n'

export interface DialogControl {
  open: boolean
  close: () => void
}

export function MergedUpdateChip() {
  const t = useT()
  const batch = useStore((s) => s.restartBatch)
  const clear = useStore((s) => s.clearRestartBatch)
  const cli = useStore((s) => s.cliUpdates)
  const readyCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.length)
  const busyCount = useStore(
    (s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).busy.filter((b) => !b.install).length,
  )
  const todo = useInstallTodo()
  const herdrPlan = useHerdrPlan()
  const herdrActive = useHerdrUpdate((s) => s.active)
  const herdrResult = useHerdrUpdate((s) => s.result)
  const herdrItem = herdrMenuItem(herdrActive, herdrResult, herdrPlan)
  const grokPlan = useGrokPlan()
  const grokItem = grokPlan ? grokMenuItem(grokPlan) : null
  const agyPlan = useAgyPlan()
  const agyBusy = useAgyBusy()
  const agyItem = agyPlan ? agyMenuItem(agyPlan) : null
  const [open, setOpen] = useState(false)
  const [at, setAt] = useState<CSSProperties>({})
  const [which, setWhich] = useState<'restart' | 'herdr' | 'grok' | 'agy' | null>(null)
  const btn = useRef<HTMLButtonElement>(null)
  const pop = useRef<HTMLDivElement>(null)
  const menuKeys = useMenuKeys(open, pop, btn, () => setOpen(false))
  useUpdateChipRequest(
    () => true,
    () => {
      const r = btn.current?.getBoundingClientRect()
      if (r) setAt({ position: 'fixed', top: Math.round(r.bottom + 6), right: 8 })
      setOpen(true)
    },
  )

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      const t = e.target as Node
      if (!btn.current?.contains(t) && !pop.current?.contains(t)) setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
  }, [open])

  const restartShown = Boolean(batch) || readyCount > 0 || busyCount > 0 || todo.count > 0 || todo.active.length > 0
  const labels = mergedUpdateLabels({
    batch,
    cli,
    readyCount,
    busyCount,
    installCount: 0,
    to: null,
    restartShown,
    codexShown: false,
    claudeShown: false,
    herdrItem,
    grokItem,
    agyItem,
  })
  const restartItem =
    todo.active.length > 0
      ? t('安裝中 {hosts}…（裝完自動重啟）', { hosts: todo.active.map((item) => `${item.host} ${item.kind}`).join(t('、')) })
      : todo.count > 0 && !batch
        ? t('安裝新版並重啟（{n} 項待辦）', { n: todo.count + readyCount + busyCount })
        : labels.restartItem
  const label = todo.count > 0 || todo.active.length > 0 ? labels.label.replace(labels.restartItem, restartItem) : labels.label
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
          setAt({ position: 'fixed', top: Math.round(r.bottom + 6), right: 8 })
          setOpen((v) => !v)
        }}
      >
        <span aria-hidden="true">
          <UpgradeIcon />
        </span>
        {}
      </button>
      {open
        ? createPortal(
            <div
              ref={pop}
              className="head-menu-pop update-menu-pop"
              style={at}
              role="menu"
              aria-label={t('更新')}
              onClick={() => setOpen(false)}
              onKeyDown={menuKeys}
            >
              {restartShown ? <button
                type="button"
                className="head-menu-item"
                role="menuitem"
                tabIndex={-1}
                disabled={!batch && todo.active.length === 0 && readyCount === 0 && busyCount === 0 && todo.count === 0}
                onClick={() => (batch ? clear() : todo.active.length > 0 ? undefined : setWhich('restart'))}
              >
                {restartItem}
              </button> : null}
              {herdrItem ? <button
                type="button"
                className="head-menu-item"
                role="menuitem"
                tabIndex={-1}
                disabled={herdrActive !== null}
                onClick={() => setWhich('herdr')}
              >
                {herdrItem}
              </button> : null}
              {grokItem ? <button
                type="button"
                className="head-menu-item"
                role="menuitem"
                tabIndex={-1}
                onClick={() => setWhich('grok')}
              >
                {grokItem}
              </button> : null}
              {agyItem ? <button
                type="button"
                className="head-menu-item"
                role="menuitem"
                tabIndex={-1}
                disabled={agyBusy}
                onClick={() => setWhich('agy')}
              >
                {agyItem}
              </button> : null}
            </div>,
            document.body,
          )
        : null}
      <RestartChip control={{ open: which === 'restart', close }} />
      <HerdrUpdateChip control={{ open: which === 'herdr', close }} />
      <GrokUpdateChip control={{ open: which === 'grok', close }} />
      <AgyUpdateChip control={{ open: which === 'agy', close }} />
    </>
  )
}
