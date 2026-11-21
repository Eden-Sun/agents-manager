import { useMemo, useState, type ReactNode } from 'react'
import type { BotKind } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import * as api from '../api'
import { KindTag } from './KindTag'
import { KIND_LABEL } from './kindMeta'
import { ConfirmDialog } from './ConfirmDialog'
import { kindBots } from '../store/identityRows'

function useKindTool(host: string, kind: BotKind) {
  return useStore((s) => (host === 'local' ? s.localTools : s.hosts.find((h) => h.name === host)?.tools)?.[kind])
}

function KindSwitch({ kind }: { kind: BotKind }) {
  const t = useT()
  const disabledKinds = useStore((s) => s.disabledKinds)
  const setKindDisabled = useStore((s) => s.setKindDisabled)
  const busy = useStore((s) => Object.keys(s.busy).some((k) => k.startsWith('kind-disabled:')))
  const bots = useStore((s) => s.bots)
  const projects = useStore((s) => s.projects)
  const [confirm, setConfirm] = useState(false)
  const affected = useMemo(() => kindBots(bots, projects, 'local', kind).length, [bots, projects, kind])
  const off = disabledKinds.includes(api.kindPrefKey('local', kind))
  return (
    <>
      {off ? <span className="identity-disabled-chip">{t('已停用')}</span> : null}
      <button
        type="button"
        className={`mini-btn identity-login-btn${off ? ' is-off' : ''}`}
        disabled={busy}
        onClick={() => (off ? void setKindDisabled('local', kind, false) : setConfirm(true))}
      >
        {off ? t('啟用本機 {kind}', { kind }) : t('停用本機 {kind}…', { kind })}
      </button>
      <ConfirmDialog
        open={confirm}
        title={t('停用本機 {kind}', { kind })}
        body={t.x('停用後本機的 {kind} 不會再被挑中、派工或啟動。目前本機有 {n} 顆 {kind} Bot：已經在跑的不受影響，之後重新啟動才會停住。', {
          kind: <code>{kind}</code>,
          n: affected,
        })}
        confirmLabel={t('停用本機 {kind}', { kind })}
        danger
        onCancel={() => setConfirm(false)}
        onConfirm={() => {
          setConfirm(false)
          void setKindDisabled('local', kind, true)
        }}
      />
    </>
  )
}

export function IdentityKindCard({ host, kind, children, onAdd }: { host: string; kind: BotKind; children: ReactNode; onAdd?: () => void }) {
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
        <span className="identity-kind-actions">
          {onAdd ? (
            <button type="button" className="mini-btn" onClick={onAdd}>
              {t('新增帳號')}
            </button>
          ) : null}
          {host === 'local' ? <KindSwitch kind={kind} /> : null}
        </span>
      </header>
      {installed ? null : <p className="hint">{tool ? t('這台主機沒有安裝 {kind}', { kind }) : t('尚未偵測 {kind} 是否安裝', { kind })}</p>}
      {children}
    </section>
  )
}
