import { useState } from 'react'
import type { Message } from '../api/types'
import { identityStatusOfHost, projectHostName, useStore } from '../store/store'
import { authActionTargetId } from '../lib/authFailure'
import { startCliLogin } from '../lib/cliLogin'
import { hostLabel } from '../store/identityRows'
import './authLoginAction.css'

/**
 * 回合因為沒登入／授權失敗收尾的那則訊息底下的「立即登入」＋「登入好了，重試」。
 * 登入走獨立主機 shell 跑 CLI（`startCliLogin`），原 bot 的 pane 不被登入流程佔住；
 * 重試＝重新偵測身份，確定不是「沒登入」才重送上一則。只掛在最後一則 auth 失敗上（`authActionTargetId`）。
 */
export function AuthLoginAction({ msg }: { msg: Message }) {
  const botId = msg.bot_id ?? null
  const isTarget = useStore((s) => (botId ? authActionTargetId(s.messages[botId] ?? []) === msg.id : false))
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  const identity = useStore((s) => s.bots.find((b) => b.id === botId)?.identity ?? null)
  const host = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === botId)?.project_id ?? null))
  const lastUserText = useStore((s) => {
    const list = botId ? (s.messages[botId] ?? []) : []
    for (let i = list.length - 1; i >= 0; i -= 1) {
      if (list[i].role === 'user') return list[i].content
    }
    return null
  })
  const agentBusy = useStore((s) => {
    const st = botId ? s.runs[botId]?.agent_status : undefined
    return st === 'working' || st === 'blocked'
  })
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  const refreshTools = useStore((s) => s.refreshTools)
  const sendPrompt = useStore((s) => s.sendPrompt)
  const notify = useStore((s) => s.notify)
  const [opening, setOpening] = useState(false)
  const [opened, setOpened] = useState(false)
  const [retrying, setRetrying] = useState(false)

  if (!botId || !isTarget || !kind) return null
  const who = identity ?? '預設帳號'
  const where = hostLabel(host)

  async function login() {
    if (!botId) return
    setOpening(true)
    const ok = await startCliLogin(botId)
    setOpening(false)
    if (!ok) return
    setOpened(true)
    notify('info', `已在${where}開終端替 ${who} 跑 ${kind} 登入，完成瀏覽器授權後回來按「登入好了，重試」`)
  }

  async function retry() {
    if (!botId) return
    setRetrying(true)
    try {
      if (!(await refreshTools(host))) return
      // 只有確定「沒登入」才擋；遠端 claude 問不出來（null）照樣放行，重送失敗會再長出一則。
      const loggedIn = identity ? identityStatusOfHost(useStore.getState(), host)[identity]?.logged_in : undefined
      if (loggedIn === false) {
        notify('error', `${who} 在${where}還是沒登入：先在登入終端完成授權`)
        return
      }
      if (!lastUserText) {
        notify('info', '已重新偵測；這個對話沒有可以重送的訊息')
        return
      }
      if (await sendPrompt(botId, lastUserText)) {
        notify('info', '已重送上一則；若還是 Not logged in，重啟這個 Bot 讓它重讀憑證')
      }
    } finally {
      setRetrying(false)
    }
  }

  return (
    <div className="auth-login-action" role="group" aria-label="登入後重試">
      <div className="auth-login-row">
        <button
          type="button"
          className="btn primary"
          disabled={opening}
          title={`在${where}開一個獨立終端，帶 ${who} 的設定跑 ${kind === 'claude' ? 'claude auth login' : `${kind} login`}`}
          onClick={() => void login()}
        >
          {opening ? '開終端中…' : '立即登入'}
        </button>
        <button
          type="button"
          className="btn"
          disabled={retrying || toolsBusy || agentBusy}
          title={
            agentBusy
              ? '它正在忙，等這一輪停下來再重試'
              : lastUserText
                ? `重新偵測 ${who}，登入了就重送：${lastUserText.slice(0, 60)}`
                : `重新偵測 ${who} 的登入狀態`
          }
          onClick={() => void retry()}
        >
          {retrying || toolsBusy ? '偵測中…' : '登入好了，重試'}
        </button>
      </div>
      <span className="auth-login-hint">
        {opened
          ? '在新開的終端完成瀏覽器授權（或把 code 貼回去），CLI 結束後終端會自己收掉。'
          : `用 CLI 在獨立終端登入 ${who}，不佔用這個 Bot 的畫面。`}
      </span>
    </div>
  )
}
