import { useMemo, useState } from 'react'
import type { BotKind } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { enabledIdentities, identitiesOfHost, identityStatusOfHost, projectHostName, toolsOfHost, useStore } from '../store/store'
import { isQuotaDisabled, quotaDisableKey, useDisabledQuota } from '../store/quotaHide'
import { loginKindOnHost } from '../lib/onboardingActions'
import { KindTag } from './KindTag'
import { useT } from '../lib/i18n'
import './quickAddBots.css'

/** 空專案的一鍵新增：只列這台主機上真的能開的 kind／已登入身份，點一下就建好並啟動。 */
type Choice = {
  key: string
  kind: BotKind
  identity: string | null
  label: string
  title: string
  loggedOut: boolean
  /** 這個身分在額度條上被勾成「暫時停用」了（見 store/quotaHide.ts）。 */
  quotaOff: boolean
}

function nextName(base: string, projectId: string, bots: { project_id: string; name: string }[]): string {
  const taken = new Set(bots.filter((b) => b.project_id === projectId).map((b) => b.name))
  let n = 1
  while (taken.has(`${base}-${n}`)) n += 1
  return `${base}-${n}`
}

export function QuickAddBots({ projectId }: { projectId: string }) {
  const t = useT()
  const bots = useStore((s) => s.bots)
  const addBot = useStore((s) => s.addBot)
  const startBot = useStore((s) => s.startBot)
  const host = useStore((s) => projectHostName(s, projectId))
  const tools = useStore((s) => toolsOfHost(s, host))
  const allIdentities = useStore((s) => s.identities)
  const status = useStore((s) => identityStatusOfHost(s, host))
  const disabledQuota = useDisabledQuota()
  const [loginPending, setLoginPending] = useState<string | null>(null)
  const refreshTools = useStore((s) => s.refreshTools)
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  const [busy, setBusy] = useState<string | null>(null)
  const hostLabel = !host || host === 'local' ? t('本機') : host

  const disabledIdentities = useStore((s) => s.disabledIdentities)
  const choices = useMemo<Choice[]>(() => {
    const out: Choice[] = []
    for (const kind of BOT_KINDS) {
      if (!tools[kind]?.installed) continue
      const ids = enabledIdentities(disabledIdentities, host, identitiesOfHost(allIdentities, status, host)).filter((i) => i.kind === kind)
      if (ids.length === 0) {
        // 沒登入的列出來，點了開登入流程（不是新增）。
        const loggedOut = tools[kind].logged_in === false
        const quotaOff = isQuotaDisabled(disabledQuota, quotaDisableKey(host, kind, null))
        out.push({
          key: kind,
          kind,
          identity: null,
          label: kind,
          title: quotaOff
            ? t('{name} 已在額度條上暫時停用——現在開一顆同樣沒額度可用', { name: kind })
            : loggedOut
              ? t('{name} 在 {host} 尚未登入——點一下開登入終端', { name: kind, host: hostLabel })
              : t('在 {host} 開一個 {kind}', { host: hostLabel, kind }),
          loggedOut,
          quotaOff,
        })
        continue
      }
      for (const i of ids) {
        const st = status[i.name]
        const loggedOut = st?.logged_in === false
        const quotaOff = isQuotaDisabled(disabledQuota, quotaDisableKey(host, kind, i.name))
        const accountStr = st?.account ? ` — ${st.account}` : ''
        const hostNote = t('（{host}）', { host: hostLabel })
        out.push({
          key: `${kind}:${i.name}`,
          kind,
          identity: i.name,
          label: i.name,
          title: quotaOff
            ? t('{name} 已在額度條上暫時停用——現在開一顆同樣沒額度可用', { name: i.name })
            : loggedOut
              ? t('{name} 在 {host} 尚未登入——點一下開登入終端', { name: i.name, host: hostLabel })
              : `${kind} · ${i.name}${accountStr}${hostNote}`,
          loggedOut,
          quotaOff,
        })
      }
    }
    return out
  }, [tools, allIdentities, status, hostLabel, host, disabledQuota, disabledIdentities, t])

  if (choices.length === 0) return null

  const pick = (c: Choice) => {
    // 未登入的開出來只會停在登入畫面：點下去直接開登入流程（issue #1931），登好後再點一次就是新增。
    if (c.loggedOut) {
      void loginKindOnHost(host, c.kind, c.identity).then((opened) => {
        if (opened && !c.identity) setLoginPending(host)
      })
      return
    }

    setBusy(c.key)
    void addBot(projectId, {
      name: nextName(c.identity ?? c.kind, projectId, bots),
      // 這裡算的名字是從瀏覽器的清單來的，剛新增完那一拍可能還沒同步；撞到就讓 daemon 往後找。
      name_auto: true,
      kind: c.kind,
      model: null,
      effort: null,
      persona: null,
      autostart: false,
      auto_approve: true,
      identity: c.identity,
    }).then(async (id) => {
      setBusy(null)
      if (id) await startBot(id)
    })
  }

  return (
    <div className="quick-add" role="group" aria-label={t('快速新增 Bot')}>
      {loginPending === host ? (
        <button type="button" className="btn identity-recheck" disabled={toolsBusy} onClick={() => void refreshTools(host).then((ok) => { if (ok) setLoginPending(null) })}>
          {toolsBusy ? t('偵測中…') : t('登入好了，重新偵測')}
        </button>
      ) : null}
      {choices.map((c) => (
        <button
          key={c.key}
          type="button"
          className={`quick-add-chip${c.quotaOff ? ' quota-off' : ''}`}
          // 停用中的身分點下去只會多一顆同樣沒額度的 bot——列出來但不給點；未登入的點了去登入。
          disabled={busy !== null || c.quotaOff}
          title={c.title}
          onClick={() => pick(c)}
        >
          <KindTag kind={c.kind} />
          <span className="quick-add-label">{busy === c.key ? t('建立中…') : c.label}</span>
          {c.quotaOff ? (
            <span className="quick-add-out">{t('已停用')}</span>
          ) : c.loggedOut ? (
            <span className="quick-add-out">{t('未登入・點此登入')}</span>
          ) : null}
        </button>
      ))}
    </div>
  )
}
