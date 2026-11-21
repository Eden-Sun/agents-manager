import { useState, type ReactNode } from 'react'
import { useStore } from '../store/store'
import { useAgyMissing } from './agyInstallState'
import { ConfirmDialog } from './ConfirmDialog'
import { hostLabel } from './toolsHelpers'
import { useT } from '../lib/i18n'

export function AgyInstallDialog({
  host,
  open,
  confirmLabel,
  extra,
  onCancel,
  onConfirm,
}: {
  host: string
  open: boolean
  confirmLabel: string
  extra?: ReactNode
  onCancel: () => void
  onConfirm: () => void
}) {
  const t = useT()
  return (
    <ConfirmDialog
      open={open}
      title={t('安裝 agy？')}
      body={
        <>
          {t.x('{host} 尚未安裝 agy，要自動安裝嗎？', { host: <strong>{hostLabel(host)}</strong> })}
          <br />
          {t.x('會從官方下載（約 50 MB）、驗 sha512 後放到 {path}；不跑官方 install.sh、不改 shell 設定。', {
            path: <code>~/.local/bin/agy</code>,
          })}
          {extra ? (
            <>
              <br />
              {extra}
            </>
          ) : null}
        </>
      }
      confirmLabel={confirmLabel}
      width={400}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}

export function AgyInstallButton({ host, small }: { host: string; small?: boolean }) {
  const t = useT()
  const missing = useAgyMissing(host)
  const busy = useStore((s) => s.busy[`agy-install:${host || 'local'}`] === true)
  const installAgy = useStore((s) => s.installAgy)
  const [open, setOpen] = useState(false)
  if (!missing && !open) return null
  return (
    <>
      <button
        type="button"
        className={`${small ? 'mini-btn' : 'btn'} install-btn`}
        disabled={busy}
        title={t('在{host}從官方下載 agy（約 50 MB、驗 sha512）並放到 ~/.local/bin/agy；不跑官方 install.sh、不改 shell 設定', { host: hostLabel(host) })}
        onClick={(e) => {
          e.stopPropagation()
          setOpen(true)
        }}
      >
        {busy ? t('安裝中…') : small ? t('安裝') : t('安裝 agy')}
      </button>
      <AgyInstallDialog
        host={host}
        open={open}
        confirmLabel={t('安裝 agy')}
        onCancel={() => setOpen(false)}
        onConfirm={() => {
          setOpen(false)
          void installAgy(host)
        }}
      />
    </>
  )
}
