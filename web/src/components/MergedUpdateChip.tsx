import { useEffect, useRef, useState, type CSSProperties } from 'react'
import { createPortal } from 'react-dom'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { claudeInstallPlan, codexInstallPlan, updateBatchCounts } from '../lib/updateBatch'
import { updateRange } from '../lib/updateRange'
import { mergedUpdateLabels } from '../lib/mergedUpdateLabels'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { UpgradeIcon } from './UpgradeIcon'
import { CodexInstallChip } from './CodexInstallChip'
import { ClaudeInstallChip } from './ClaudeInstallChip'
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
  const installCount = useStore(
    (s) =>
      codexInstallPlan(
        s.bots,
        s.runs,
        (id) => inFlightTurn(s, id) !== null,
        (b) => projectHostName(s, b.project_id),
        s.upstreamUpdates.codex,
      )?.installCount ?? 0,
  )
  const codexNotice = useStore(
    (s) =>
      codexInstallPlan(
        s.bots,
        s.runs,
        (id) => inFlightTurn(s, id) !== null,
        (b) => projectHostName(s, b.project_id),
        s.upstreamUpdates.codex,
      )?.notice ?? '',
  )
  const claudePlanKey = useStore((s) => {
    const p = claudeInstallPlan(s.upstreamUpdates.claude)
    return p ? JSON.stringify(p) : ''
  })
  const claudePlan = claudePlanKey ? (JSON.parse(claudePlanKey) as NonNullable<ReturnType<typeof claudeInstallPlan>>) : null
  const activeClaudeHosts = new Set(cli.filter((item) => item.kind === 'claude').map((item) => item.host))
  const claudeCanStart = Boolean(claudePlan?.installHosts.some((host) => !activeClaudeHosts.has(host)))
  const claudeVisible = Boolean(claudePlan) || activeClaudeHosts.size > 0
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
  const [which, setWhich] = useState<'restart' | 'codex' | 'claude' | 'herdr' | 'grok' | 'agy' | null>(null)
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

  const restartShown = Boolean(batch) || readyCount > 0 || busyCount > 0
  const codexShown = cli.some((item) => item.kind === 'codex') || codexNotice !== ''
  const { restartItem, codexItem, claudeItem, label } = mergedUpdateLabels({
    batch,
    cli,
    readyCount,
    busyCount,
    installCount,
    to: updateRange('codex', codexNotice, null).to,
    claudeInstallCount: claudePlan?.installHosts.length ?? 0,
    claudeTarget: claudePlan?.target ?? null,
    claudeShown: claudeVisible,
    claudeSummary: claudePlan?.hosts.map((host) => `${host.host} ${host.installedVersion ?? host.error ?? t('讀取失敗')} → ${claudePlan.target}`).join(t('；')),
    restartShown,
    codexShown,
    herdrItem,
    grokItem,
    agyItem,
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
                disabled={!batch && readyCount === 0 && busyCount === 0}
                onClick={() => (batch ? clear() : setWhich('restart'))}
              >
                {restartItem}
              </button> : null}
              {codexShown ? <button
                type="button"
                className="head-menu-item"
                role="menuitem"
                tabIndex={-1}
                disabled={cli.some((item) => item.kind === 'codex')}
                onClick={() => setWhich('codex')}
              >
                {codexItem}
              </button> : null}
              {claudeVisible ? <button
                type="button"
                className="head-menu-item"
                role="menuitem"
                tabIndex={-1}
                disabled={!claudeCanStart}
                onClick={() => setWhich('claude')}
              >
                {claudeItem}
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
      <CodexInstallChip control={{ open: which === 'codex', close }} />
      <ClaudeInstallChip control={{ open: which === 'claude', close }} />
      <HerdrUpdateChip control={{ open: which === 'herdr', close }} />
      <GrokUpdateChip control={{ open: which === 'grok', close }} />
      <AgyUpdateChip control={{ open: which === 'agy', close }} />
    </>
  )
}
