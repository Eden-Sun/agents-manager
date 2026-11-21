import { useState, type ReactNode } from 'react'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'
import { openFreshShellAndType } from '../lib/cliLogin'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'
import { AgyInstallDialog } from './AgyInstall'
import { useAgyMissing } from './agyInstallState'

export function QuotaLoginShell({
  host,
  hostLabel,
  kind,
  command,
  guide,
}: {
  host: string
  hostLabel: string
  kind: BotKind
  command: string
  guide?: ReactNode
}) {
  const t = useT()
  const [open, setOpen] = useState(false)
  const [sent, setSent] = useState(false)
  const refreshTools = useStore((s) => s.refreshTools)
  const notify = useStore((s) => s.notify)
  const shellBusy = useStore((s) => s.busy[`shell:${host}`] === true)
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  const installAgy = useStore((s) => s.installAgy)
  const installBusy = useStore((s) => s.busy[`agy-install:${host || 'local'}`] === true)
  const agyMissing = useAgyMissing(host) && kind === 'agy'

  async function run() {
    if (!(await useStore.getState().confirmCodexLoginReplace(host, kind, null))) return
    const { opened, typed } = await openFreshShellAndType(host, command)
    if (!opened) return
    setSent(true)
    if (typed) notify('info', t('已在 {host} 的 shell 送出 {command}，請在那個終端完成登入', { host: hostLabel, command }))
  }

  return (
    <div className="bs-login-row">
      <button
        type="button"
        className="btn"
        disabled={shellBusy || installBusy}
        title={
          agyMissing
            ? t('{host} 尚未安裝 agy：先自動安裝，再開 shell 登入', { host: hostLabel })
            : t('在 {host} 開一個 shell 並輸入 {command}', { host: hostLabel, command })
        }
        onClick={() => setOpen(true)}
      >
        {installBusy ? t('安裝中…') : shellBusy ? t('開 shell 中…') : agyMissing ? t('安裝 agy 並登入') : t('開 shell 登入')}
      </button>
      {sent ? (
        <button
          type="button"
          className="identity-recheck"
          disabled={toolsBusy}
          title={t('重新問 {host} 上的 CLI 現在登入了誰', { host: hostLabel })}
          onClick={() => void refreshTools(host)}
        >
          {toolsBusy ? t('偵測中…') : t('登入好了，重新偵測')}
        </button>
      ) : null}

      <AgyInstallDialog
        host={host}
        open={open && agyMissing}
        confirmLabel={t('安裝並登入')}
        extra={t.x('裝好後會接著在 {host} 開 shell 並送出 {cmd} 讓你登入。', { host: hostLabel, cmd: <code>{command}</code> })}
        onCancel={() => setOpen(false)}
        onConfirm={() => {
          setOpen(false)
          void installAgy(host).then((ok) => {
            if (ok) void run()
          })
        }}
      />
      <ConfirmDialog
        open={open && !agyMissing}
        title={t('開 shell 登入 {kind}？', { kind })}
        body={
          <>
            {t.x('會在 {host} 開一個 shell，畫面切過去，並送出 {cmd}，', {
              host: <strong>{hostLabel}</strong>,
              cmd: <code>{command}</code>,
            })}
            {guide ?? t('通常會跳出瀏覽器要你在那邊完成登入。')}
            <br />
            <strong>{t('在你完成登入之前，這個身份的 {kind} 還是不能工作。', { kind })}</strong>
            <br />
            {t('登入完成後回到這裡按「登入好了，重新偵測」，身份狀態才會更新。')}
          </>
        }
        confirmLabel={t('開 shell 並送出')}
        width={400}
        onCancel={() => setOpen(false)}
        onConfirm={() => {
          setOpen(false)
          void run()
        }}
      />
    </div>
  )
}
