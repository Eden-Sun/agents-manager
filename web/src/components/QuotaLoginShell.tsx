import { useState } from 'react'
import type { BotKind } from '../api/types'
import * as api from '../api'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'

/**
 * 額度 popover「未登入」列的開 shell 登入：codex 沒有 `/login`，或 claude / grok 沒有在跑的 Bot。
 * CLI 狀態不會自己回來，所以送出後補「重新偵測」（同 `BotSettingsPanel`）。
 */
export function QuotaLoginShell({
  host,
  hostLabel,
  kind,
  command,
}: {
  host: string
  /** 已經算好的主機顯示名（本機 / 主機名），免得這裡再抄一份規則。 */
  hostLabel: string
  kind: BotKind
  command: string
}) {
  const [open, setOpen] = useState(false)
  const [sent, setSent] = useState(false)
  const openHostShell = useStore((s) => s.openHostShell)
  const refreshTools = useStore((s) => s.refreshTools)
  const notify = useStore((s) => s.notify)
  const shellBusy = useStore((s) => s.busy[`shell:${host}`] === true)
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)

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
        disabled={shellBusy}
        title={`在 ${hostLabel} 開一個 shell 並輸入 ${command}`}
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

      <ConfirmDialog
        open={open}
        title={`開 shell 登入 ${kind}？`}
        body={
          <>
            會在 <strong>{hostLabel}</strong> 開一個 shell，畫面切過去，並送出 <code>{command}</code>，通常會跳出瀏覽器要你在那邊完成登入。
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
