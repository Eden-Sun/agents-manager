import { useCallback, useEffect, useMemo, useState } from 'react'
import { fetchIntegrationUpdates, installIntegration, integrationErrText, type IntegrationHost } from '../api/integrationUpdates'
import { useHerdrPlan } from '../hooks/useHerdrPlan'
import { herdrHostBlocked, startHerdr, useHerdrUpdate } from '../store/herdrUpdate'
import { useStore } from '../store/store'
import { STATUS_LABEL, buildHostCheck, summaryText, updatableCount, type CheckAction, type CheckRow, type HostCheck } from '../store/updateCheck'
import { ConfirmDialog } from './ConfirmDialog'
import { AgyUpdateBody } from './AgyUpdateConfirm'
import { agyUpdateTitle } from '../lib/agyUpdate'
import { t, useT } from '../lib/i18n'
import { UpstreamChangelogBox } from './UpstreamChangelogBox'
import { useReviewAsk } from '../hooks/useReviewAsk'
import { changelogRange } from '../lib/upstreamRange'
import './updateCheck.css'

/**
 * 設定頁「更新檢查」（issue #1772）：每台主機一張表，列出所有要檢查升級的項目——herdr、各 agent CLI、herdr integration。
 * 這張表是總覽：資料與動作都沿用既有的（header 徽章、`cli-update`、`herdr-update`），只有 integration 是新的；
 * integration 的「更新」只在這裡按才會在那台跑 `herdr integration install`，沒裝的只顯示（grok／agy 已登入時 daemon 會自動裝一次）。
 */
export function UpdateCheckPanel() {
  const t = useT()
  const hosts = useStore((s) => s.hosts)
  const localTools = useStore((s) => s.localTools)
  const localHerdr = useStore((s) => s.localHerdr)
  const upstream = useStore((s) => s.upstreamUpdates)
  const cliUpdates = useStore((s) => s.cliUpdates)
  const startCliUpdate = useStore((s) => s.startCliUpdate)
  const installAgy = useStore((s) => s.installAgy)
  const agyBusy = useStore((s) => s.busy)
  const notify = useStore((s) => s.notify)
  const herdrPlan = useHerdrPlan()
  const herdrActive = useHerdrUpdate((s) => s.active)

  const [snap, setSnap] = useState<IntegrationHost[] | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [busy, setBusy] = useState<Set<string>>(new Set())
  const [ask, setAsk] = useState<{ host: string; row: CheckRow; action: CheckAction } | null>(null)
  // agy 的確認框有「請 AGM 解析」（#1928）；只有按下去才會花 API，開框只讀。
  const agyAsk = ask?.action.kind === 'agy' ? { host: ask.host, from: changelogRange([{ host: ask.host, from: ask.row.current }]).from, target: ask.action.target } : null
  const agyReview = useReviewAsk('agy', agyAsk?.host ?? 'local', agyAsk?.from ?? null, agyAsk?.target ?? null)

  // 舊 daemon 沒有這支：其他列照常顯示，只有 integration 那段講「讀不到」。
  const load = useCallback(
    () =>
      fetchIntegrationUpdates().then(
        (hosts) => {
          setSnap(hosts)
          setLoadError(null)
        },
        (e: unknown) => setLoadError(integrationErrText(e, 'local')),
      ),
    [],
  )
  useEffect(() => {
    void load()
  }, [load])

  const checks: HostCheck[] = useMemo(() => {
    const shared = new Set(hosts.filter((h) => h.shared_session).map((h) => h.name))
    const one = (host: string, tools: Parameters<typeof buildHostCheck>[0]['tools'], herdr: Parameters<typeof buildHostCheck>[0]['herdr']) =>
      buildHostCheck({
        host,
        tools,
        herdr,
        upstream,
        integration: snap?.find((h) => h.host === host) ?? null,
        herdrBlocked: herdrHostBlocked(host, shared),
      })
    return [one('local', localTools, localHerdr), ...hosts.map((h) => one(h.name, h.tools, h.herdr))]
  // `t` 進依賴：切語言時 buildHostCheck 裡的說明文字要重算。
  }, [hosts, localTools, localHerdr, upstream, snap, t])

  const running = (host: string, a: CheckAction): boolean => {
    if (a.kind === 'cli') return cliUpdates.some((u) => u.host === host && u.kind === a.cliKind)
    if (a.kind === 'herdr') return herdrActive !== null
    if (a.kind === 'agy') return agyBusy[`agy-install:${host || 'local'}`] === true
    return busy.has(`${host}:${a.name}`)
  }

  const run = async () => {
    if (!ask) return
    const { host, action } = ask
    setAsk(null)
    if (action.kind === 'cli') {
      try {
        await startCliUpdate(host, action.cliKind, action.target)
      } catch (e) {
        notify('error', t('{kind} 更新沒開始：{reason}', { kind: action.cliKind, reason: integrationErrText(e, host) }))
      }
    } else if (action.kind === 'herdr') {
      await startHerdr(host, action.target, notify)
    } else if (action.kind === 'agy') {
      await installAgy(host)
    } else {
      const key = `${host}:${action.name}`
      setBusy((b) => new Set(b).add(key))
      try {
        const r = await installIntegration(host, action.name)
        if (r.state === 'current') {
          notify(
            'info',
            action.fresh
              ? t('{host} 的 {name} integration 已安裝', { host, name: action.name })
              : t('{host} 的 {name} integration 已更新', { host, name: action.name }),
          )
        } else if (r.state === null) {
          notify('error', t('{host} 的 {name} integration 裝完重讀失敗：{reason}', { host, name: action.name, reason: r.rereadError ?? t('讀不到狀態') }))
        } else {
          notify('error', t('{host} 的 {name} integration 裝完還不是最新（{state}）', { host, name: action.name, state: r.state }))
        }
      } catch (e) {
        notify(
          'error',
          action.fresh
            ? t('{host} 的 {name} integration 安裝失敗：{reason}', { host, name: action.name, reason: integrationErrText(e, host) })
            : t('{host} 的 {name} integration 更新失敗：{reason}', { host, name: action.name, reason: integrationErrText(e, host) }),
        )
      } finally {
        setBusy((b) => {
          const next = new Set(b)
          next.delete(key)
          return next
        })
        void load()
      }
    }
  }

  return (
    <div className="update-check">
      <p className="hint">
        {t.x(
          '每台主機一張表：herdr、各 agent CLI、herdr integration。這裡是總覽，更新動作沿用既有入口；integration 只在你按「更新」時才會在那台跑 {cmd}；沒裝的只顯示，只有 grok／agy 已登入時 daemon 會自動裝一次、也可以按「安裝」。',
          { cmd: <code>herdr integration install</code> },
        )}
      </p>
      {loadError ? <p className="update-check-err" role="status">{t('integration 狀態讀不到：{error}', { error: loadError })}</p> : null}
      {checks.map((c) => (
        <HostTable key={c.host} check={c} loadFailed={loadError !== null} running={running} onAsk={(row, action) => setAsk({ host: c.host, row, action })} />
      ))}
      <div className="update-check-foot">
        <button type="button" className="btn" onClick={() => void load()}>
          {t('重新讀取')}
        </button>
      </div>
      <ConfirmDialog
        open={ask !== null}
        title={ask ? confirmTitle(ask.row, ask.action) : ''}
        danger={ask?.action.kind === 'herdr'}
        confirmLabel={ask?.action.kind === 'integration' && ask.action.fresh ? t('安裝') : t('更新')}
        {...(agyAsk ? agyReview.dialog : {})}
        width={420}
        body={ask ? <ConfirmBody host={ask.host} row={ask.row} action={ask.action} willResume={herdrPlan?.host === ask.host ? herdrPlan.willResume.map((b) => b.name) : []} reviewKey={agyReview.reviewKey} /> : null}
        onCancel={() => setAsk(null)}
        onConfirm={() => void run()}
      />
    </div>
  )
}

function confirmTitle(row: CheckRow, action: CheckAction): string {
  if (action.kind === 'integration') return action.fresh ? t('安裝 {label}？', { label: row.label }) : t('更新 {label}？', { label: row.label })
  if (action.kind === 'agy') return agyUpdateTitle(action.target)
  return t('更新 {label} 到 {target}？', { label: row.label, target: action.target })
}

function ConfirmBody({ host, row, action, willResume, reviewKey }: { host: string; row: CheckRow; action: CheckAction; willResume: string[]; reviewKey: number }) {
  const t = useT()
  if (action.kind === 'integration' && action.fresh) {
    return (
      <>
        {t.x('在 {host} 跑 {cmd}，第一次安裝 hook 腳本。會改那台 agent 的 hook 設定檔（grok 在 {grok}、agy 在 {agy}）。', {
          host: <strong>{host}</strong>,
          cmd: <code>herdr integration install {action.name}</code>,
          grok: <code>~/.grok</code>,
          agy: <code>~/.gemini</code>,
        })}
      </>
    )
  }
  if (action.kind === 'integration') {
    return (
      <>
        {t.x('在 {host} 跑 {cmd}，把 hook 腳本從 {from} 換成 {to}。會改那台 agent 的設定檔（例如 {claude}、{codex}）。', {
          host: <strong>{host}</strong>,
          cmd: <code>herdr integration install {action.name}</code>,
          from: row.current ?? '?',
          to: row.latest ?? '?',
          claude: <code>~/.claude</code>,
          codex: <code>~/.codex</code>,
        })}
      </>
    )
  }
  if (action.kind === 'agy') {
    const from = changelogRange([{ host, from: row.current }]).from
    return (
      <>
        <UpstreamChangelogBox kind="agy" host={host} from={from} to={action.target} refreshKey={reviewKey} />
        <p>
          <AgyUpdateBody host={host} current={row.current} target={action.target} />
        </p>
      </>
    )
  }
  if (action.kind === 'herdr') {
    return (
      <>
        {t.x('{bold}，子 agent 一律消失、要由母 Bot 重開。', { bold: <strong>{t('herdr server 會整個重啟，所有 Bot 中斷約 1 分鐘')}</strong> })}
        {willResume.length > 0 ? <> {t('重啟後接回：{names}。', { names: willResume.join(t('、')) })}</> : null}
      </>
    )
  }
  return (
    <>
      {t.x('在 {host} 安裝 {label} {target}（{current} → {target}）。裝好後依既有流程套用，Bot 不會被這個按鈕直接重啟。', {
        host: <strong>{host}</strong>,
        label: row.label,
        target: action.target,
        current: row.current ?? '?',
      })}
    </>
  )
}

function HostTable({
  check,
  loadFailed,
  running,
  onAsk,
}: {
  check: HostCheck
  loadFailed: boolean
  running: (host: string, a: CheckAction) => boolean
  onAsk: (row: CheckRow, action: CheckAction) => void
}) {
  const t = useT()
  const n = updatableCount(check.rows)
  const hostName = check.host === 'local' ? t('本機') : check.host
  return (
    <section className="update-host" aria-label={t('{host} 更新檢查', { host: hostName })}>
      <h4 className="update-host-name">
        {hostName}
        <span className={`update-host-count${n > 0 ? ' has' : ''}`}>{summaryText(check)}</span>
      </h4>
      <div className="update-rows" role="table">
        <div className="update-row update-head" role="row">
          <span role="columnheader">{t('項目')}</span>
          <span role="columnheader">{t('目前')}</span>
          <span role="columnheader">{t('最新／需要')}</span>
          <span role="columnheader">{t('狀態')}</span>
          <span role="columnheader">{t('動作')}</span>
        </div>
        {check.rows.map((r) => (
          <div key={r.id} className={`update-row st-${r.status}`} role="row" data-row={r.id}>
            <span className="update-label" role="cell">{r.label}</span>
            <span className="update-cur" role="cell" data-k={t('目前')}>{r.current ?? '—'}</span>
            <span className="update-lat" role="cell" data-k={t('最新')}>{r.latest ?? '—'}</span>
            <span className="update-st" role="cell"><span className={`update-badge ${r.status}`}>{t.tr(STATUS_LABEL[r.status])}</span></span>
            <span className="update-act" role="cell">
              {r.action ? (
                <button type="button" className="btn" disabled={running(check.host, r.action)} onClick={() => onAsk(r, r.action!)}>
                  {running(check.host, r.action)
                    ? (r.action.kind === 'integration' && r.action.fresh ? t('安裝中…') : t('更新中…'))
                    : (r.action.kind === 'integration' && r.action.fresh ? t('安裝…') : t('更新…'))}
                </button>
              ) : null}
              {r.note ? <span className="update-note">{r.note}</span> : null}
            </span>
          </div>
        ))}
        {check.integrationPending && !loadFailed ? (
          <div className="update-row update-pending" role="row"><span className="update-note">{t('integration：還沒讀到這台的狀態（daemon 開機後約 1 分鐘第一輪）')}</span></div>
        ) : null}
        {check.integrationError ? (
          <div className="update-row update-pending" role="row"><span className="update-note">{t('integration：這一輪讀不到（{error}），上面是上次的結果', { error: check.integrationError })}</span></div>
        ) : null}
      </div>
    </section>
  )
}
