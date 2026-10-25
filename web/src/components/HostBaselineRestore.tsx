import { useState } from 'react'
import * as api from '../api'
import type { HostBaselineBackup, HostBaselineBackups, HostBaselineRestoreResult } from '../api/types'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'

const ERROR_CODES = new Set([
  'backup_list_failed',
  'host_superseded',
  'backup_not_found',
  'invalid_backup_id',
  'restore_refused',
  'not_found',
  'unavailable',
  'upstream',
])

function errorCode(error: unknown): string {
  if (!(error instanceof ApiError)) return 'operation_failed'
  const raw = String(error.body.reason ?? error.body.error ?? '')
  return ERROR_CODES.has(raw) ? raw : 'operation_failed'
}

function errorMessage(t: ReturnType<typeof useT>, code: string): string {
  const message = (() => {
    switch (code) {
      case 'backup_list_failed': return t('備份清單讀取失敗')
      case 'host_superseded': return t('主機連線已重置或改指')
      case 'backup_not_found': return t('找不到這份備份')
      case 'invalid_backup_id': return t('備份識別碼無效')
      case 'restore_refused': return t('備份驗證失敗，未還原')
      case 'not_found': return t('找不到主機或備份')
      case 'unavailable': return t('主機暫時無法操作')
      case 'upstream': return t('主機操作失敗')
      default: return t('操作失敗')
    }
  })()
  return t('操作失敗（{code}）：{message}', { code, message })
}

function resultText(t: ReturnType<typeof useT>, result: string): string {
  switch (result) {
    case 'restored': return t('已還原')
    case 'deleted': return t('已刪除')
    case 'absent': return t('原本就不存在')
    default: return t('還原失敗')
  }
}

export interface HostBaselineRestoreProps {
  host: string
}

/** 可還原備份的 metadata 與破壞性操作確認；不依賴 host_baseline_sync。 */
export function HostBaselineRestore({ host }: HostBaselineRestoreProps) {
  const t = useT()
  const [list, setList] = useState<HostBaselineBackups | null>(null)
  const [selected, setSelected] = useState<HostBaselineBackup | null>(null)
  const [confirmOpen, setConfirmOpen] = useState(false)
  const [loading, setLoading] = useState(false)
  const [restoring, setRestoring] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [restoreResult, setRestoreResult] = useState<HostBaselineRestoreResult | null>(null)

  const loadBackups = async () => {
    setLoading(true)
    setList(null)
    setError(null)
    setRestoreResult(null)
    try {
      const backups = await api.fetchHostBaselineBackups(host)
      if (!backups) throw new Error('invalid response')
      setList(backups)
    } catch (e) {
      setError(errorMessage(t, errorCode(e)))
    } finally {
      setLoading(false)
    }
  }

  const startRestore = (backup: HostBaselineBackup) => {
    setSelected(backup)
    setError(null)
    setRestoreResult(null)
    setConfirmOpen(true)
  }

  const confirmRestore = async () => {
    if (!selected || restoring) return
    setRestoring(true)
    setError(null)
    try {
      const result = await api.restoreHostBaseline(host, selected.backup_id)
      if (!result) throw new Error('invalid response')
      setRestoreResult(result)
      setConfirmOpen(false)
      setSelected(null)
    } catch (e) {
      setError(errorMessage(t, errorCode(e)))
    } finally {
      setRestoring(false)
    }
  }

  const cancelRestore = () => {
    if (restoring) return
    setConfirmOpen(false)
    setSelected(null)
    setError(null)
  }

  const overwriteFiles = selected?.files.filter((file) => file.restore === 'overwrite') ?? []
  const deleteFiles = selected?.files.filter((file) => file.restore === 'delete') ?? []

  return (
    <span className="host-baseline-restore-wrap">
      <button
        type="button"
        className="mini-btn host-baseline-restore-open"
        disabled={loading || restoring}
        onClick={() => void loadBackups()}
      >
        {loading ? t('讀取備份中…') : t('查看備份／還原')}
      </button>

      {error && !confirmOpen ? <span className="host-baseline-restore-error" role="alert">{error}</span> : null}

      {list ? (
        <span className="host-baseline-restore-list">
          {list.skipped > 0 ? (
            <span className="host-baseline-restore-warning" role="status">
              {t('有 {n} 份備份無法讀取', { n: list.skipped })}
            </span>
          ) : null}
          {list.backups.length === 0 ? <span>{t('沒有可還原的備份')}</span> : null}
          <ul>
            {list.backups.map((backup) => (
              <li key={backup.backup_id} className="host-baseline-restore-entry">
                <time dateTime={backup.created_at}>{backup.created_at}</time>
                <span className="host-baseline-restore-id">{backup.backup_id}</span>
                <ul>
                  {backup.files.map((file) => (
                    <li key={file.file}>{t('{file}：{action}', { file: file.file, action: file.restore === 'overwrite' ? t('還原會覆寫') : t('還原會刪除') })}</li>
                  ))}
                </ul>
                <button type="button" className="mini-btn" disabled={loading || restoring} onClick={() => startRestore(backup)}>
                  {t('還原')}
                </button>
              </li>
            ))}
          </ul>
        </span>
      ) : null}

      {restoreResult ? (
        <span className="host-baseline-restore-summary" role="status">
          <span>{t('還原結果：{id}', { id: restoreResult.backup_id })}</span>
          <ul>
            {restoreResult.files.map((file, index) => (
              <li key={`${file.file}-${index}`}>{t('{file}：{action}', { file: file.file, action: resultText(t, file.result) })}</li>
            ))}
          </ul>
        </span>
      ) : null}

      <ConfirmDialog
        open={confirmOpen}
        title={t('確認還原主機備份')}
        width={500}
        danger
        body={(
          <div className="host-baseline-restore-dialog">
            {selected ? (
              <>
                <div>{t('主機：{name}', { name: host })}</div>
                <div>{t('備份時間：{time}', { time: selected.created_at })}</div>
                <div className="host-baseline-restore-section-head">{t('會覆寫的檔案：')}</div>
                {overwriteFiles.length ? <ul>{overwriteFiles.map((file) => <li key={file.file}>{file.file}</li>)}</ul> : <div>{t('沒有')}</div>}
                <div className="host-baseline-restore-section-head">{t('會刪除的檔案（備份時不存在）：')}</div>
                {deleteFiles.length ? <ul>{deleteFiles.map((file) => <li key={file.file}>{file.file}</li>)}</ul> : <div>{t('沒有')}</div>}
                <p className="host-baseline-restore-notice">{t('還原前不會另外備份目前狀態；會直接覆寫或刪除上列檔案。')}</p>
              </>
            ) : null}
            {error ? <div className="host-baseline-restore-error" role="alert">{error}</div> : null}
          </div>
        )}
        confirmLabel={restoring ? t('還原中…') : t('確認還原')}
        cancelLabel={t('取消')}
        confirmDisabled={restoring || !selected}
        onConfirm={() => void confirmRestore()}
        onCancel={cancelRestore}
      />
    </span>
  )
}
