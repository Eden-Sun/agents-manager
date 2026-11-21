import { useStore } from '../store/store'
import type { RestartBatch } from '../api/types'
import { useT } from '../lib/i18n'
import './updateAllBanner.css'

export function UpdateAllBanner() {
  const batch = useStore((s) => s.restartBatch)
  const clear = useStore((s) => s.clearRestartBatch)
  if (!batch) return null
  return <BatchRow batch={batch} onClear={clear} />
}

export function BatchRow({ batch, onClear }: { batch: RestartBatch; onClear: () => void }) {
  const t = useT()
  const { total, done, current, ok, failed, skipped, finished } = batch
  const unknownTotal = total <= 0
  const pct = total > 0 ? Math.round((done / total) * 100) : 0

  return (
    <div className={`update-all running${finished ? ' done' : ''}`}>
      <div className="update-all-line">
        <strong>
          {finished
            ? `${t('重啟完成 · 成功 {ok} 顆', { ok: ok.length })}${skipped.length ? t(' · 跳過 {skipped} 顆', { skipped: skipped.length }) : ''}${
                failed.length ? t(' · 失敗 {failed} 顆', { failed: failed.length }) : ''
              }`
            : `${t('重啟中 {progress}', { progress: unknownTotal ? t('{done} 顆（總數未知）', { done }) : `${done}/${total}` })}${current ? ` · ${current}` : ''}`}
        </strong>
        {}
        <button
          type="button"
          className="update-all-x"
          aria-label={finished ? t('收起這則摘要') : t('收起進度（不會中斷重啟）')}
          onClick={onClear}
        >
          ✕
        </button>
      </div>
      {finished ? null : (
        <div
          className={`update-all-bar${unknownTotal ? ' unknown' : ''}`}
          role="progressbar"
          aria-valuemin={0}
          {...(unknownTotal ? {} : { 'aria-valuenow': done, 'aria-valuemax': total })}
        >
          <span style={unknownTotal ? undefined : { width: `${pct}%` }} />
        </div>
      )}
      {}
      {failed.length > 0 ? (
        <ul className="update-all-list bad">
          {failed.map((f) => (
            <li key={f.name}>
              <b>{f.name}</b>
              {f.error}
            </li>
          ))}
        </ul>
      ) : null}
      {finished && skipped.length > 0 ? (
        <ul className="update-all-list">
          {skipped.map((sk) => (
            <li key={sk.bot_id}>
              <b>{sk.name}</b>
              {sk.reason_label}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}
