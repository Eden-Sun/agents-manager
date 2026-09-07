import { useState } from 'react'
import * as api from '../api'
import { codexLoginCommand } from '../lib/quotaLogin'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'

/**
 * 額度 popover 裡 codex 那一列的「未登入」動作。
 *
 * codex 的 TUI 沒有 `/login`，所以這裡不像 claude / grok 那樣對 Bot 送 slash 指令，而是
 * 在該主機開一個 shell（`store.openHostShell`）再把 `codex login` 打進去——使用者只要看著
 * 那個終端把瀏覽器那段做完就好，不必自己找路。daemon 完全沒動。
 *
 * 登入完成後 CLI 的狀態不會自己回來，所以送出後補一顆「登入好了，重新偵測」，跟
 * `BotSettingsPanel` 的那顆同一個行為（`refreshTools(host)`、同一個 busy key）。
 */
export function QuotaLoginCodex({
  host,
  hostLabel,
  identity,
}: {
  host: string
  /** 已經算好的主機顯示名（本機 / 主機名），免得這裡再抄一份規則。 */
  hostLabel: string
  identity: string | null
}) {
  const [open, setOpen] = useState(false)
  const [sent, setSent] = useState(false)
  const openHostShell = useStore((s) => s.openHostShell)
  const refreshTools = useStore((s) => s.refreshTools)
  const notify = useStore((s) => s.notify)
  const supported = useStore((s) => s.hostShellSupported)
  const shellBusy = useStore((s) => s.busy[`shell:${host}`] === true)
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  const command = useStore((s) =>
    codexLoginCommand(identity ? s.identities.find((i) => i.kind === 'codex' && i.name === identity) : undefined),
  )

  async function run() {
    if (!(await openHostShell(host))) return
    const view = useStore.getState().shellView
    if (!view) return
    try {
      await api.sendHostShellText(host, view.paneId, command, true)
      setSent(true)
      notify('info', `已在 ${hostLabel} 的 shell 送出 ${command}，請在那個終端完成登入`)
    } catch {
      // 打字失敗不代表 shell 沒開起來：使用者眼前就是那個終端，告訴他自己敲哪一行最省事。
      setSent(true)
      notify('info', `shell 已開好，請在裡面輸入 ${command}`)
    }
  }

  return (
    <div className="bs-login-row">
      <button
        type="button"
        className="btn"
        disabled={!supported || shellBusy}
        title={
          supported
            ? `在 ${hostLabel} 開一個 shell 並輸入 ${command}`
            : '這版 daemon 沒有主機 shell，請自己在終端跑 codex login'
        }
        onClick={() => setOpen(true)}
      >
        {shellBusy ? '開 shell 中…' : '開 shell 登入'}
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

      {/* 按下去會換畫面（shell 佔掉主區），而且登入沒完成之前這個身份的 codex 還是不能用。
          這兩件事先講，按鈕才誠實——同 BotSettingsPanel 那顆 `/login` 的作法。 */}
      <ConfirmDialog
        open={open}
        title="開 shell 登入 codex？"
        body={
          <>
            會在 <strong>{hostLabel}</strong> 開一個 shell，畫面切過去，並送出 <code>{command}</code>，通常會跳出瀏覽器要你在那邊完成登入。
            <br />
            <strong>在你完成登入之前，這個身份的 codex 還是不能工作。</strong>
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
