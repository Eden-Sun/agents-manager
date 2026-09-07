import { useState } from 'react'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'
import { canLoginInSession, cliLoginCommand, findLoginTargetId, identityEnv } from '../lib/quotaLogin'
import { QuotaLoginShell } from './QuotaLoginShell'
import { ConfirmDialog } from './ConfirmDialog'

/**
 * 額度 popover 裡「這個帳號未登入」那一列的登入按鈕（claude 與 grok：TUI 有 `/login` 的 kind）。
 *
 * 以前這裡只寫一句「請在 Bot 設定按登入 / 切換帳號」，使用者得自己去翻是哪一個 bot。
 * 這些 kind 的 TUI 有 `/login`，那就直接在這裡送——目標是同 host、同 kind、同身份、還在跑的那個 bot
 * （`findLoginTarget`）。沒有在跑的就 disabled，因為 `/login` 要送進活著的 pane。
 */
export function QuotaLoginSlash({
  kind,
  host,
  hostLabel,
  identity,
}: {
  kind: BotKind
  host: string
  hostLabel: string
  identity: string | null
}) {
  const botId = useStore((s) => findLoginTargetId(s, host, kind, identity))
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? null)
  const loginBot = useStore((s) => s.loginBot)
  const refreshTools = useStore((s) => s.refreshTools)
  const notify = useStore((s) => s.notify)
  const loginBusy = useStore((s) => (botId ? s.busy[`login:${botId}`] === true : false))
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  const [confirmOpen, setConfirmOpen] = useState(false)
  /** 送出過一次之後才冒出「重新偵測」——沒送過就沒有東西需要重新偵測。 */
  const [loginSent, setLoginSent] = useState(false)
  const shellCommand = useStore((s) => cliLoginCommand(kind, identityEnv(s, host, kind, identity)))

  // codex 沒有 `/login`，走 `QuotaLogin-codex`；這裡擋一下免得被誤用時給出壞按鈕。
  if (!canLoginInSession(kind)) return null
  // 沒有正在跑的 Bot 就沒有畫面可以送 `/login`。與其給一顆灰掉的按鈕叫使用者先去開 bot，
  // 不如走 codex 那條路：開該主機的 shell 跑 `<cli> login`，登的是同一個身份。
  if (!botId) return <QuotaLoginShell host={host} hostLabel={hostLabel} kind={kind} command={shellCommand} />

  return (
    <div className="bs-login-row">
      <button
        type="button"
        className="btn"
        disabled={!botId || loginBusy}
        title={
          botId
            ? `對 ${botName} 的 ${kind} 送 /login`
            : '這個身份沒有正在跑的 Bot，先啟動一個'
        }
        onClick={() => setConfirmOpen(true)}
      >
        {loginBusy ? '送出中…' : '登入 / 切換帳號'}
      </button>
      {loginSent ? (
        <button
          type="button"
          className="identity-recheck"
          disabled={toolsBusy}
          title="重新問這台主機上的 CLI 現在登入了誰"
          onClick={() => void refreshTools(host)}
        >
          {toolsBusy ? '偵測中…' : '登入好了，重新偵測'}
        </button>
      ) : null}

      {/* 送 `/login` 不是「按了就登入好了」：畫面會跑到 agent 那邊，而且在使用者完成之前
          那個 Bot 不能工作。這三件事在按下去**之前**講清楚，按鈕才誠實。 */}
      <ConfirmDialog
        open={confirmOpen}
        title="送出登入指令？"
        body={
          <>
            會對 <strong>{botName}</strong> 的 agent 送 <code>/login</code>。它的畫面會切到登入流程，通常會開瀏覽器要你在那邊完成登入。
            <br />
            <strong>在你完成登入之前，這個 Bot 不能工作</strong>——這期間送給它的訊息會卡住。
            <br />
            登入完成後回到這裡按「重新偵測」，身份狀態才會更新。
          </>
        }
        confirmLabel="送出 /login"
        width={400}
        onCancel={() => setConfirmOpen(false)}
        onConfirm={() => {
          setConfirmOpen(false)
          if (!botId) return
          void loginBot(botId).then((ok) => {
            if (!ok) return
            setLoginSent(true)
            notify('info', `已送出 /login 給 ${botName}，請到它的畫面完成登入`)
          })
        }}
      />
    </div>
  )
}
