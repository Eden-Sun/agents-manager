import type { ReactNode } from 'react'
import type { BotKind } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import * as api from '../api'
import { KindTag } from './KindTag'
import { KIND_LABEL } from './kindMeta'

/** 某台主機某個 kind 的工具狀態（本機用 localTools，遠端用 hosts[].tools）。 */
function useKindTool(host: string, kind: BotKind) {
  return useStore((s) => (host === 'local' ? s.localTools : s.hosts.find((h) => h.name === host)?.tools)?.[kind])
}

/** kind 啟用開關（只在本機）：停用＝不挑、不派、不啟動、不探測；已經在跑的 Bot 不殺。 */
function KindSwitch({ kind }: { kind: BotKind }) {
  const t = useT()
  const disabledKinds = useStore((s) => s.disabledKinds)
  const setKindDisabled = useStore((s) => s.setKindDisabled)
  const busy = useStore((s) => Object.keys(s.busy).some((k) => k.startsWith('kind-disabled:')))
  const off = disabledKinds.includes(api.kindPrefKey('local', kind))
  return (
    <>
      {off ? <span className="identity-disabled-chip">{t('已停用')}</span> : null}
      <button type="button" className={`mini-btn identity-login-btn${off ? ' is-off' : ''}`} disabled={busy} onClick={() => void setKindDisabled('local', kind, !off)}>
        {off ? t('啟用') : t('停用')}
      </button>
    </>
  )
}

/**
 * 一張 CLI 卡：標頭是 kind（圖示＋名稱）與（本機）啟用開關；內容依序是預設帳號、自訂身份、shell 偵測身份。
 * 沒安裝時標一行說明；身份（綁著它的 config 身份）仍照列。
 */
export function IdentityKindCard({ host, kind, children }: { host: string; kind: BotKind; children: ReactNode }) {
  const t = useT()
  const tool = useKindTool(host, kind)
  const installed = tool?.installed === true
  return (
    <section className="identity-kind-card" data-kind-card={`${host}-${kind}`} aria-label={kind}>
      <header className="identity-kind-card-head">
        <span className="identity-kind-title">
          <KindTag kind={kind} />
          <span className="identity-kind-name">{KIND_LABEL[kind]}</span>
        </span>
        {host === 'local' ? <KindSwitch kind={kind} /> : null}
      </header>
      {installed ? null : <p className="hint">{tool ? t('這台主機沒有安裝 {kind}', { kind }) : t('尚未偵測 {kind} 是否安裝', { kind })}</p>}
      {children}
    </section>
  )
}
