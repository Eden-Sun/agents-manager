import { useState, type ReactNode } from 'react'
import { useStore } from '../store/store'
import { useAgyMissing } from './agyInstallState'
import { ConfirmDialog } from './ConfirmDialog'
import { hostLabel } from './toolsHelpers'

/**
 * 「<主機> 尚未安裝 agy，要自動安裝嗎？」的確認框（使用者 2026-10-06）。agy 不能跑官方 `install.sh`，由 daemon 讀官方 manifest、
 * 驗 sha512 後放到那台的 `~/.local/bin/agy`（`POST /api/hosts/{name}/agy/install`）；使用者按確認才會動手。`extra` 接在說明後面
 * （例如「裝好後接著開 shell 登入」）。
 */
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
  return (
    <ConfirmDialog
      open={open}
      title="安裝 agy？"
      body={
        <>
          <strong>{hostLabel(host)}</strong> 尚未安裝 agy，要自動安裝嗎？
          <br />
          會從官方下載（約 50 MB）、驗 sha512 後放到 <code>~/.local/bin/agy</code>；不跑官方 install.sh、不改 shell 設定。
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

/**
 * 沒裝 agy 的主機：安裝鈕（缺少 CLI 提示、主機徽章、新 bot 的 kind 選單都用它）。不需要先有一顆在跑的 bot，
 * 跟其他 CLI 的「用現有 agent 安裝」不同。按下去先跳確認，確認才裝；已經裝了就不顯示（登入在額度欄「開 shell 登入」）。
 */
export function AgyInstallButton({ host, small }: { host: string; small?: boolean }) {
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
        title={`在${hostLabel(host)}從官方下載 agy（約 50 MB、驗 sha512）並放到 ~/.local/bin/agy；不跑官方 install.sh、不改 shell 設定`}
        onClick={(e) => {
          e.stopPropagation()
          setOpen(true)
        }}
      >
        {busy ? '安裝中…' : small ? '安裝' : '安裝 agy'}
      </button>
      <AgyInstallDialog
        host={host}
        open={open}
        confirmLabel="安裝 agy"
        onCancel={() => setOpen(false)}
        onConfirm={() => {
          setOpen(false)
          void installAgy(host)
        }}
      />
    </>
  )
}
