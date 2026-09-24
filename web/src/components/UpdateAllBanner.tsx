import { useStore } from '../store/store'
import type { RestartBatch } from '../api/types'
import './updateAllBanner.css'

/** 「claude 有更新」按下去之後側欄的進度／失敗與跳過名單（SPEC §6.9）；觸發鈕在 `UpdateQuotaChip`。 */
export function UpdateAllBanner() {
  const batch = useStore((s) => s.restartBatch)
  const clear = useStore((s) => s.clearRestartBatch)
  if (!batch) return null
  return <BatchRow batch={batch} onClear={clear} />
}

/** 收 props（不自己讀 store）才測得到，同 `JudgeLoadNotice`／`BotSwitcherMenu`。 */
export function BatchRow({ batch, onClear }: { batch: RestartBatch; onClear: () => void }) {
  const { total, done, current, ok, failed, skipped, finished } = batch
  const pct = total > 0 ? Math.round((done / total) * 100) : 100

  return (
    <div className={`update-all running${finished ? ' done' : ''}`}>
      <div className="update-all-line">
        <strong>
          {finished
            ? `重啟完成 · 成功 ${ok.length} 顆${skipped.length ? ` · 跳過 ${skipped.length} 顆` : ''}${
                failed.length ? ` · 失敗 ${failed.length} 顆` : ''
              }`
            : `重啟中 ${done}/${total}${current ? ` · ${current}` : ''}`}
        </strong>
        {/* 未完成也要關得掉（issue #492）：`bots_restart_done` 收不到（批次中途 daemon 重啟、或落到全量
            resync）時，以前這裡沒有 ✕、晶片又是停用的，使用者只能重新整理分頁。收起不會中斷批次——
            它在 daemon 背景跑，再按一次會拿到 `already_running` 並重新接上同一批。 */}
        <button
          type="button"
          className="update-all-x"
          aria-label={finished ? '收起這則摘要' : '收起進度（不會中斷重啟）'}
          onClick={onClear}
        >
          ✕
        </button>
      </div>
      {finished ? null : (
        <div className="update-all-bar" role="progressbar" aria-valuenow={done} aria-valuemin={0} aria-valuemax={total}>
          <span style={{ width: `${pct}%` }} />
        </div>
      )}
      {/* 失敗的先列：那是要動手的。跳過的接在後面，理由用 daemon 給的那句。 */}
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
