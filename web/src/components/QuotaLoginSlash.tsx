import { useState } from 'react'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { AGY_LOGIN_GUIDE, canLoginInSession, cliLoginCommand, findLoginTargetId, identityEnv, shouldUseIdentityLogin } from '../lib/quotaLogin'
import { QuotaLoginShell } from './QuotaLoginShell'
import { ConfirmDialog } from './ConfirmDialog'

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
  const t = useT()
  const botId = useStore((s) => findLoginTargetId(s, host, kind, identity))
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? null)
  const loginBot = useStore((s) => s.loginBot)
  const refreshTools = useStore((s) => s.refreshTools)
  const notify = useStore((s) => s.notify)
  const loginBusy = useStore((s) => (botId ? s.busy[`login:${botId}`] === true : false))
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  const [confirmOpen, setConfirmOpen] = useState(false)
  const [loginSent, setLoginSent] = useState(false)
  const shellCommand = useStore((s) => cliLoginCommand(kind, identityEnv(s, host, kind, identity)))

  if (shouldUseIdentityLogin(identity)) return <IdentityCliLogin kind={kind} host={host} hostLabel={hostLabel} identity={identity} />
  if (kind === 'claude') return <QuotaLoginShell host={host} hostLabel={hostLabel} kind={kind} command={shellCommand} />
  if (kind === 'agy') return <QuotaLoginShell host={host} hostLabel={hostLabel} kind={kind} command={shellCommand} guide={t.tr(AGY_LOGIN_GUIDE)} />
  if (!canLoginInSession(kind)) return null
  if (!botId) return <QuotaLoginShell host={host} hostLabel={hostLabel} kind={kind} command={shellCommand} />

  return (
    <div className="bs-login-row">
      <button
        type="button"
        className="btn"
        disabled={!botId || loginBusy}
        title={botId ? t('對 {bot} 的 {kind} 送 /login', { bot: botName ?? '', kind }) : t('這個身份沒有正在跑的 Bot，先啟動一個')}
        onClick={() => setConfirmOpen(true)}
      >
        {loginBusy ? t('送出中…') : t('登入 / 切換帳號')}
      </button>
      {loginSent ? (
        <button
          type="button"
          className="identity-recheck"
          disabled={toolsBusy}
          title={t('重新問這台主機上的 CLI 現在登入了誰')}
          onClick={() => void refreshTools(host)}
        >
          {toolsBusy ? t('偵測中…') : t('登入好了，重新偵測')}
        </button>
      ) : null}

      <ConfirmDialog
        open={confirmOpen}
        title={t('送出登入指令？')}
        body={
          <>
            {t.x('會對 {bot} 的 agent 送 {cmd}。它的畫面會切到登入流程，通常會開瀏覽器要你在那邊完成登入。', {
              bot: <strong>{botName}</strong>,
              cmd: <code>/login</code>,
            })}
            <br />
            <strong>{t('在你完成登入之前，這個 Bot 不能工作')}</strong>
            {t('——這期間送給它的訊息會卡住。')}
            <br />
            {t('登入完成後回到這裡按「重新偵測」，身份狀態才會更新。')}
          </>
        }
        confirmLabel={t('送出 /login')}
        width={400}
        onCancel={() => setConfirmOpen(false)}
        onConfirm={() => {
          setConfirmOpen(false)
          if (!botId) return
          void loginBot(botId).then((ok) => {
            if (!ok) return
            setLoginSent(true)
            notify('info', t('已送出 /login 給 {bot}，請到它的畫面完成登入', { bot: botName ?? '' }))
          })
        }}
      />
    </div>
  )
}

export function IdentityCliLogin({ kind, host, hostLabel, identity }: { kind: BotKind; host: string; hostLabel: string; identity: string }) {
  const t = useT()
  const loginIdentity = useStore((s) => s.loginIdentity)
  const busy = useStore((s) => s.busy[`identity-login:${host}:${identity}`] === true)
  const command = kind === 'claude' ? 'claude auth login' : `${kind} login`
  return (
    <div className="bs-login-row">
      <button
        type="button"
        className="btn"
        disabled={busy}
        title={t('在 {host} 開獨立終端，帶 {identity} 的設定跑 {command}', { host: hostLabel, identity, command })}
        onClick={() => void loginIdentity(host, identity)}
      >
        {busy ? t('開終端中…') : t('開終端登入')}
      </button>
    </div>
  )
}
