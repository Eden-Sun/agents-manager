import { useState, type ReactNode } from 'react'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'
import { openFreshShellAndType } from '../lib/cliLogin'
import { ConfirmDialog } from './ConfirmDialog'
import { AgyInstallDialog } from './AgyInstall'
import { useAgyMissing } from './agyInstallState'

/**
 * 額度 popover「未登入」列的開 shell 登入：codex 沒有 `/login`，或 claude / grok 沒有在跑的 Bot。
 * CLI 狀態不會自己回來，所以送出後補「重新偵測」（同 `BotSettingsPanel`）。
 */
export function QuotaLoginShell({
  host,
  hostLabel,
  kind,
  command,
  guide,
}: {
  host: string
  /** 已經算好的主機顯示名（本機 / 主機名），免得這裡再抄一份規則。 */
  hostLabel: string
  kind: BotKind
  command: string
  /** 這個 CLI 的登入過程跟「跳出瀏覽器」不一樣時，換掉確認框裡那段說明（agy：TUI 自己引導）。 */
  guide?: ReactNode
}) {
  const [open, setOpen] = useState(false)
  const [sent, setSent] = useState(false)
  const refreshTools = useStore((s) => s.refreshTools)
  const notify = useStore((s) => s.notify)
  const shellBusy = useStore((s) => s.busy[`shell:${host}`] === true)
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  const installAgy = useStore((s) => s.installAgy)
  const installBusy = useStore((s) => s.busy[`agy-install:${host || 'local'}`] === true)
  // 這台沒有 agy：開 shell 只會得到 `command not found`，先問要不要自動安裝，裝好才接著登入（使用者 2026-10-06）。
  const agyMissing = useAgyMissing(host) && kind === 'agy'

  async function run() {
    // 一律新開（不接回舊 shell，#915）：指令不會被打進別人前景的 vim／sudo／手動開的 claude。
    const { opened, typed } = await openFreshShellAndType(host, command)
    if (!opened) return
    setSent(true)
    if (typed) notify('info', `已在 ${hostLabel} 的 shell 送出 ${command}，請在那個終端完成登入`)
  }

  return (
    <div className="bs-login-row">
      <button
        type="button"
        className="btn"
        disabled={shellBusy || installBusy}
        title={agyMissing ? `${hostLabel} 尚未安裝 agy：先自動安裝，再開 shell 登入` : `在 ${hostLabel} 開一個 shell 並輸入 ${command}`}
        onClick={() => setOpen(true)}
      >
        {installBusy ? '安裝中…' : shellBusy ? '開 shell 中…' : agyMissing ? '安裝 agy 並登入' : '開 shell 登入'}
      </button>
      {sent ? (
        <button
          type="button"
          className="identity-recheck"
          disabled={toolsBusy}
          title={`重新問 ${hostLabel} 上的 CLI 現在登入了誰`}
          onClick={() => void refreshTools(host)}
        >
          {toolsBusy ? '偵測中…' : '登入好了，重新偵測'}
        </button>
      ) : null}

      <AgyInstallDialog
        host={host}
        open={open && agyMissing}
        confirmLabel="安裝並登入"
        extra={<>裝好後會接著在 {hostLabel} 開 shell 並送出 <code>{command}</code> 讓你登入。</>}
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
        title={`開 shell 登入 ${kind}？`}
        body={
          <>
            會在 <strong>{hostLabel}</strong> 開一個 shell，畫面切過去，並送出 <code>{command}</code>，
            {guide ?? '通常會跳出瀏覽器要你在那邊完成登入。'}
            <br />
            <strong>在你完成登入之前，這個身份的 {kind} 還是不能工作。</strong>
            <br />
            登入完成後回到這裡按「登入好了，重新偵測」，身份狀態才會更新。
          </>
        }
        confirmLabel="開 shell 並送出"
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
