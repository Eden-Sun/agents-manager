import { useState } from 'react'
import type { ShellIdentityDeleteOutcome, ShellIdentityDeleteRef } from '../store/store'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import type { BotKind } from '../api/types'
import { hostLabel, shadowedByConfig } from '../store/identityRows'
import { ConfirmDialog } from './ConfirmDialog'
import { Cc0Note, DisabledChip, IdentityBadge, IdentityHostLogins } from './IdentityRow'

function shellDeleteReasonText(outcome: ShellIdentityDeleteOutcome, t: ReturnType<typeof useT>) {
  switch (outcome.reason) {
    case 'alias_backup_unconfirmed': return t('無法確認備份是否建立，原設定檔未修改；請檢查回傳的預定備份位置。')
    case 'alias_delete_unconfirmed': return outcome.backup_path
      ? t('無法確認 alias 移除結果；請檢查回傳的備份位置與目前設定。')
      : t('無法確認 alias 移除結果，且未收到備份路徑；請先檢查該主機目前的設定。')
    case 'invalid_shell_identity': return t('只支援 Claude cc0 到 cc6 shell alias。')
    case 'identity_source_mismatch': return t('找不到這台主機上可直接移除的 shell alias；設定身份沒有變更。')
    case 'identity_in_use': return t('這個身份仍被 Bot、執行中的 Run、巡檢或 supervisor role 使用，shell alias 沒有移除。')
    case 'alias_source_unknown': return t('無法確認 alias 定義在哪個 shell 設定檔，沒有移除。')
    case 'alias_definition_unsupported': return t('alias 寫法不支援安全移除，沒有移除。')
    case 'alias_definition_duplicate': return t('設定檔中有多個同名 alias，沒有移除。')
    case 'alias_file_changed': return t('設定檔內容已變更，移除未完成；請檢查目前設定與備份。')
    case 'alias_probe_failed': return t('修改後的 shell 檢查未通過；請查看備份路徑與目前設定。')
    case 'alias_still_defined': return t('重新檢查後 alias 仍存在，移除未完成。')
    case 'alias_new_reference': return t('刪除期間有參照開始使用這個身份，已嘗試還原 alias。')
    case 'alias_restore_conflict': return t('設定檔已有其他變更，未覆蓋檔案；備份仍保留。')
    default: return outcome.message || t('移除 shell alias 失敗。')
  }
}

function shellDeleteRefText(ref: ShellIdentityDeleteRef, t: ReturnType<typeof useT>) {
  const id = ref.id ?? ''
  switch (ref.type) {
    case 'bot': return t('Bot {id}', { id })
    case 'run':
    case 'runtime': return t('執行中的 Run {id}', { id })
    case 'supervisor': return t('巡檢 {id}', { id })
    case 'supervisor_role': return t('Supervisor role {id}', { id })
    default: return id ? t('{type} {id}', { type: ref.type, id }) : ref.type
  }
}

export function ShellIdentityDeleteNotice({ result }: { result: { host: string; identity: string; outcome: ShellIdentityDeleteOutcome } }) {
  const t = useT()
  const { host, identity, outcome } = result
  return (
    <div className="hint identity-shell-delete-result" role={outcome.ok ? 'status' : 'alert'}>
      {!outcome.ok ? <strong>{t('主機：{host}；Claude shell alias：{identity}', { host: hostLabel(host), identity })}</strong> : null}
      <p>
        {outcome.ok
          ? t.x('已從{host}移除 Claude shell alias {identity}。登入憑證、設定目錄與對話歷史都保留。', {
              host: <strong>{hostLabel(host)}</strong>, identity: <strong>{identity}</strong>,
            })
          : shellDeleteReasonText(outcome, t)}
      </p>
      {outcome.refs?.length ? (
        <>
          <p>{t('仍在使用這個身份的參照：')}</p>
          <ul>{outcome.refs.map((ref, index) => <li key={`${ref.type}:${ref.id ?? index}`}>{shellDeleteRefText(ref, t)}</li>)}</ul>
        </>
      ) : null}
      {outcome.backup_path ? (
        <p>{['alias_delete_unconfirmed', 'alias_backup_unconfirmed'].includes(outcome.reason ?? '') ? t('回傳的備份路徑（未確認）：') : t('保留的備份路徑：')} <code style={{ overflowWrap: 'anywhere' }}>{outcome.backup_path}</code></p>
      ) : null}
      {outcome.ok ? <p>{t('外部編輯器可能在內容比對到檔案替換之間修改設定；這個操作無法和外部編輯器同步鎖定。')}</p> : null}
    </div>
  )
}

export function ShellIdentityDeleteButton({
  host,
  identity,
  onResult,
}: {
  host: string
  identity: string
  onResult: (outcome: ShellIdentityDeleteOutcome) => void
}) {
  const t = useT()
  const removeShellIdentity = useStore((s) => s.removeShellIdentity)
  const [confirm, setConfirm] = useState(false)
  const [busy, setBusy] = useState(false)
  return (
    <>
      <button
        type="button"
        className="mini-btn identity-login-btn"
        aria-label={t('移除 {host} 的 shell alias {identity}', { host: hostLabel(host), identity })}
        title={t('從{host}的 shell 設定移除 claude alias {identity}（只移除 alias，登入與設定目錄保留）', { host: hostLabel(host), identity })}
        disabled={busy}
        onClick={() => setConfirm(true)}
      >
        {busy ? t('移除中…') : t('從 shell 設定移除…')}
      </button>
      <ConfirmDialog
        open={confirm}
        title={t('從 shell 設定移除：{host} · claude · {identity}', { host: hostLabel(host), identity })}
        body={
          <>
            {t.x('這會從{host}的 shell 設定移除 Claude alias {identity}。只移除 alias；登入憑證、設定目錄與對話歷史都會保留。', {
              host: <strong>{hostLabel(host)}</strong>, identity: <strong>{identity}</strong>,
            })}
            <br />
            {t('操作會保留僅此帳號可存取的備份並回報路徑，不會自動刪除。')}
            <br />
            {t('外部編輯器可能在內容比對到檔案替換之間修改設定；這個操作無法和外部編輯器同步鎖定。')}
          </>
        }
        confirmLabel={t('從 shell 設定移除')}
        danger
        onCancel={() => setConfirm(false)}
        onConfirm={() => {
          if (busy) return
          setConfirm(false)
          setBusy(true)
          void removeShellIdentity(host, identity)
            .then(onResult)
            .finally(() => setBusy(false))
        }}
      />
    </>
  )
}

export function ShellIdentityRows({ host, kind }: { host: string; kind: BotKind }) {
  const t = useT()
  const configured = useStore((s) => s.identities)
  const statuses = useStore((s) => (host === 'local' ? s.localIdentityStatus : s.hosts.find((h) => h.name === host)?.identity_status) ?? {})
  const [deleteResult, setDeleteResult] = useState<{ host: string; identity: string; outcome: ShellIdentityDeleteOutcome } | null>(null)
  const rows = Object.values(statuses).filter((st) => st.source === 'shell' && st.kind === kind && !shadowedByConfig(configured, host, st.name))
  return (
    <>
      {deleteResult ? <ShellIdentityDeleteNotice result={deleteResult} /> : null}
      {rows.map((st) => (
        <div className="identity-row is-shell" key={st.name} data-shell-host={host} data-shell-identity={st.name}>
          <span className="identity-main">
            <span className="identity-name">
              <IdentityBadge name={st.name} />
              <span className="identity-source-tag">{t('shell alias')}</span>
              <span className="identity-info" tabIndex={0} title={t('從登入 shell 的 {alias} alias 認出來的身份（{rc} 等），可以直接指派給 Bot；要改 alias 內容請編輯該主機的 shell 設定。「不再提供給新 Bot」只會把它從身份選單拿掉，「從 shell 設定移除」則會刪掉支援的 Claude ccN alias。', { alias: 'ccN', rc: '~/.bashrc、~/.zshrc' })} aria-label={t('說明')}>ⓘ</span>
              <DisabledChip host={host} kind={st.kind} name={st.name} />
            </span>
            <details className="identity-path">
              <summary>{t('路徑與 env')}</summary>
              <code>{st.config_dir ? `CLAUDE_CONFIG_DIR=${st.config_dir}` : t('預設帳號（無 CLAUDE_CONFIG_DIR）')}</code>
            </details>
            <Cc0Note name={st.name} kind={st.kind} />
            <IdentityHostLogins
              name={st.name}
              kind={st.kind}
              only={host}
              danger={
                st.kind === 'claude' && /^cc[0-6]$/.test(st.name) ? (
                  <ShellIdentityDeleteButton host={host} identity={st.name} onResult={(outcome) => setDeleteResult({ host, identity: st.name, outcome })} />
                ) : null
              }
            />
          </span>
        </div>
      ))}
    </>
  )
}
