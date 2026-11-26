import { useStore } from '../store/store'
import { LOCAL_HOST } from '../api/types'
import { browsersLine, TABS_WARN, tabsTotal } from '../lib/browserMem'
import { MemPopover } from './MemPopover'
import { humanBytes } from './memFormat'
import { useT } from '../lib/i18n'
import './memBadge.css'

export function MemBadge({ host = LOCAL_HOST }: { host?: string }) {
  const t = useT()
  const row = useStore((s) => s.mem?.hosts.find((h) => h.host === host) ?? null)
  const remote = host !== LOCAL_HOST
  if (!row) return null

  if (row.error) {
    return (
      <span className="mem-badge partial" title={t('{host}：量不到 herdr 的記憶體（{error}）', { host, error: row.error })}>
        {remote ? <span className="mem-k">@{host}</span> : null}
        <span className="mem-v">—</span>
      </span>
    )
  }
  if (row.total_bytes === 0 && row.processes === 0) return null

  const tabs = tabsTotal(row.browsers)
  const tabsHot = tabs >= TABS_WARN
  const machine = row.machine
  const freePct = machine && machine.total_bytes > 0 ? (machine.available_bytes / machine.total_bytes) * 100 : null
  const low = freePct !== null && freePct < 15
  const cpuPct = machine && typeof machine.cpu_pct === 'number' && Number.isFinite(machine.cpu_pct) ? Math.round(machine.cpu_pct) : null
  const cpuHot = cpuPct !== null && cpuPct >= 85
  const hostLabel = remote ? host : t('本機')
  const tip = [
    t('{host}：herdr 進程樹現在佔用的記憶體', { host: hostLabel }),
    t('herdr 本身 {herdr} · 底下的 pane 與 CLI {agents}', { herdr: humanBytes(row.herdr_bytes), agents: humanBytes(row.agents_bytes) }),
    t('{n} 個 process', { n: row.processes }),
    ...(machine
      ? [
          t('這台機器：剩 {free} / 共 {total}（已用 {used}，剩 {pct}%）', {
            free: humanBytes(machine.available_bytes),
            total: humanBytes(machine.total_bytes),
            used: humanBytes(machine.total_bytes - machine.available_bytes),
            pct: Math.round(freePct ?? 0),
          }),
          ...(cpuPct !== null ? [t('整機 CPU：{pct}%', { pct: cpuPct })] : []),
          ...(low ? [t('剩餘不到 15%，再開 bot 之前先關一些東西。')] : []),
        ]
      : []),
    ...(row.browsers.length
      ? [
          `${t('瀏覽器：{info}', { info: browsersLine(row.browsers) })}${
            tabsHot ? t('（超過 {warn} 個分頁，關一些）', { warn: TABS_WARN }) : ''
          }`,
        ]
      : []),
    '',
    t('算的是「跑在 herdr pane 裡的一切」，不只是這裡管的 bot。'),
    t('每 15 秒更新一次。'),
  ]
  return (
    <MemPopover host={host}>
      <span
        className={`mem-badge${remote ? ' remote' : ''}${tabsHot ? ' tabs-hot' : ''}${low ? ' mem-low' : ''}${cpuHot ? ' cpu-hot' : ''}`}
        title={`${tip.join('\n')}\n\n${t('點一下看有哪些程序。')}`}
      >
        <span className="mem-k">{remote ? `@${host}` : 'RAM'}</span>
        <span className="mem-v">{humanBytes(row.total_bytes)}</span>
        {machine ? (
          <span
            className="mem-free"
            title={t('這台機器還可用 {available}，共 {total}', {
              available: humanBytes(machine.available_bytes),
              total: humanBytes(machine.total_bytes),
            })}
          >
            {t('剩 {bytes}', { bytes: humanBytes(machine.available_bytes) })}
          </span>
        ) : null}
        {cpuPct !== null ? (
          <span className={`mem-cpu${cpuHot ? ' hot' : ''}`} title={t('整機 CPU：{pct}%', { pct: cpuPct })}>
            <span className="mem-cpu-sep" aria-hidden="true">· </span>
            <span className="mem-cpu-k">CPU </span>
            {cpuPct}%
          </span>
        ) : null}
        {tabsHot ? <span className="mem-tabs" aria-label={t('{n} 個瀏覽器分頁', { n: tabs })}>⧉{tabs}</span> : null}
      </span>
    </MemPopover>
  )
}
