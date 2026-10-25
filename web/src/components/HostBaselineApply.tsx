import { useState } from 'react'
import * as api from '../api'
import type { HostBaseline, HostBaselinePlan, HostBaselineApplyResult } from '../api/types'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'

export interface HostBaselineApplyProps {
  host: string
  baseline: HostBaseline | null
  syncMode?: 'off' | 'ask' | 'auto' | string
}

/** 錯誤碼到固定文案的對照，不顯示主機上的任何檔案內容或任意回傳字串。 */
function errorText(t: ReturnType<typeof useT>, code: string): string {
  switch (code) {
    case 'sync_disabled':
      return t('未開啟自動補齊（[host_baseline.sync]）')
    case 'baseline_unknown':
      return t('主機尚未完成一致性探測')
    case 'plan_outdated':
      return t('計畫已更新，請重新確認')
    case 'not_fixable':
      return t('包含無法自動補齊的項目')
    case 'host_superseded':
      return t('主機連線已重置或改指')
    case 'backup_failed':
      return t('主機備份失敗，未寫入變更')
    case 'no_home':
      return t('找不到主機家目錄')
    case 'not_found':
      return t('找不到主機')
    default:
      return t('操作失敗（{code}）', { code })
  }
}

/** 單項結果 result 到固定文案 */
function itemResultText(t: ReturnType<typeof useT>, res: string, reason?: string): string {
  if (res === 'applied') return t('已補齊')
  if (res === 'present') return t('已存在')
  if (res === 'skipped') {
    if (reason === 'file_missing') return t('略過（檔案不存在）')
    if (reason === 'symlink') return t('略過（符號連結）')
    if (reason === 'invalid_json') return t('略過（JSON 格式錯誤）')
    if (reason === 'conflict') return t('略過（結構衝突）')
    if (reason === 'write_failed') return t('略過（寫入失敗）')
    if (reason === 'backup_failed') return t('略過（備份失敗）')
    if (reason === 'aborted') return t('略過（操作中斷）')
    return t('略過')
  }
  if (res === 'manual') {
    if (reason === 'no_jq') return t('需手動（缺少 jq 工具）')
    if (reason === 'unsupported_identity') return t('需手動（自訂設定目錄）')
    if (reason === 'unsupported_key') return t('需手動（不支援的設定鍵）')
    if (reason === 'unsupported_value') return t('需手動（不支援的設定值）')
    if (reason === 'secret') return t('需手動（機密資料）')
    return t('需手動')
  }
  return res
}

export function HostBaselineApply({ host, baseline, syncMode = 'ask' }: HostBaselineApplyProps) {
  const t = useT()
  const [loading, setLoading] = useState(false)
  const [confirmOpen, setConfirmOpen] = useState(false)
  const [plan, setPlan] = useState<HostBaselinePlan | null>(null)
  const [applying, setApplying] = useState(false)
  const [errorMsg, setErrorMsg] = useState<string | null>(null)
  const [applyResult, setApplyResult] = useState<HostBaselineApplyResult | null>(null)

  // sync=off 時不畫鈕
  const effectiveMode = syncMode === 'auto' ? 'ask' : syncMode
  if (effectiveMode !== 'ask') {
    return null
  }

  // fixable 項目才可補
  const fixableCount = baseline?.fixable?.length ?? 0
  if (fixableCount === 0) {
    return null
  }

  const openConfirm = async () => {
    setErrorMsg(null)
    setApplyResult(null)
    setLoading(true)
    try {
      const p = await api.fetchHostBaselinePlan(host)
      setPlan(p)
      setConfirmOpen(true)
    } catch (e) {
      if (e instanceof ApiError) {
        const code = String(e.body.reason ?? e.body.error ?? 'error')
        setErrorMsg(errorText(t, code))
      } else {
        setErrorMsg(t('載入計畫失敗'))
      }
    } finally {
      setLoading(false)
    }
  }

  const handleConfirm = async () => {
    if (!plan) return
    setApplying(true)
    setErrorMsg(null)
    try {
      const mergeActions = plan.actions.filter((a) => a.action === 'merge_key')
      const ids = mergeActions.map((a) => a.id)
      const res = await api.applyHostBaseline(host, { plan_rev: plan.plan_rev, ids })
      setApplyResult(res)
      setConfirmOpen(false)
    } catch (e) {
      if (e instanceof ApiError) {
        const code = String(e.body.reason ?? e.body.error ?? 'error')
        if (e.status === 409 && code === 'plan_outdated') {
          // ② 409（plan_rev 過期）：重新抓計畫，顯示「計畫已更新，請重新確認」，不准自動重送 apply。
          try {
            const freshPlan = await api.fetchHostBaselinePlan(host)
            setPlan(freshPlan)
          } catch {
            // 抓新計畫失敗維持舊計畫
          }
          setErrorMsg(t('計畫已更新，請重新確認'))
          return
        }
        setErrorMsg(errorText(t, code))
      } else {
        setErrorMsg(t('套用失敗'))
      }
      setConfirmOpen(false)
    } finally {
      setApplying(false)
    }
  }

  const mergeActions = plan ? plan.actions.filter((a) => a.action === 'merge_key') : []

  const confirmBody = (
    <div className="host-baseline-apply-dialog">
      <div className="host-baseline-apply-host">
        {t('主機：{name}', { name: host })}
      </div>
      <div className="host-baseline-apply-notice">
        {t('會先備份、只補不蓋')}
      </div>
      {errorMsg ? (
        <div className="host-baseline-apply-err" role="alert">
          {errorMsg}
        </div>
      ) : null}
      <div className="host-baseline-apply-items-head">
        {t('預計補齊項目：')}
      </div>
      <ul className="host-baseline-apply-items">
        {mergeActions.map((a) => (
          <li key={a.id} className="host-baseline-apply-item">
            <span className="apply-item-id">{a.id}</span>
            <span className="apply-item-target">{t('目標：{target}', { target: a.target })}</span>
          </li>
        ))}
      </ul>
    </div>
  )

  return (
    <span className="host-baseline-apply-wrap">
      <button
        type="button"
        className="mini-btn host-baseline-apply-btn"
        disabled={loading || applying}
        onClick={() => void openConfirm()}
      >
        {loading ? t('讀取中…') : t('補齊 {n} 項', { n: fixableCount })}
      </button>

      <ConfirmDialog
        open={confirmOpen}
        title={t('確認補齊主機設定')}
        body={confirmBody}
        confirmLabel={applying ? t('套用中…') : t('確認補齊')}
        cancelLabel={t('取消')}
        confirmDisabled={applying || mergeActions.length === 0}
        onConfirm={() => void handleConfirm()}
        onCancel={() => {
          setConfirmOpen(false)
          setErrorMsg(null)
        }}
      />

      {errorMsg && !confirmOpen ? (
        <span className="host-baseline-apply-err" role="alert">
          {errorMsg}
        </span>
      ) : null}

      {applyResult ? (
        <span className="host-baseline-apply-summary">
          {applyResult.backup_id ? (
            <span className="apply-backup-id">
              {t('備份 ID：{id}', { id: applyResult.backup_id })}
              <span className="apply-restore-hint">
                {t('（如需還原請使用命令列）')}
              </span>
            </span>
          ) : null}
          <ul className="apply-result-list">
            {applyResult.results.map((r) => (
              <li key={r.id} className={`apply-res-${r.result}`}>
                <span>{r.id}</span>: <span>{itemResultText(t, r.result, r.reason)}</span>
              </li>
            ))}
          </ul>
        </span>
      ) : null}
    </span>
  )
}
