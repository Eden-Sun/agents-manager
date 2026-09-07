import { useMemo, useState } from 'react'
import type { BotKind } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { identitiesOfHost, identityStatusOfHost, projectHostName, toolsOfHost, useStore } from '../store/store'
import { KindTag } from './KindTag'

/**
 * 空專案的一鍵新增：直接列出「這台主機上真的能開」的選項（claude 的每個已登入身份
 * cc0/cc1/…，加上其他已安裝的 kind），點一下就建好並啟動，省掉開表單挑 kind 挑身份。
 * 不能用的（CLI 未安裝、身份沒登入）不列，列出來的都保證可行。
 */
type Choice = { key: string; kind: BotKind; identity: string | null; label: string; title: string }

function nextName(base: string, projectId: string, bots: { project_id: string; name: string }[]): string {
  const taken = new Set(bots.filter((b) => b.project_id === projectId).map((b) => b.name))
  let n = 1
  while (taken.has(`${base}-${n}`)) n += 1
  return `${base}-${n}`
}

export function QuickAddBots({ projectId }: { projectId: string }) {
  const bots = useStore((s) => s.bots)
  const addBot = useStore((s) => s.addBot)
  const startBot = useStore((s) => s.startBot)
  const host = useStore((s) => projectHostName(s, projectId))
  const tools = useStore((s) => toolsOfHost(s, host))
  const allIdentities = useStore((s) => s.identities)
  const status = useStore((s) => identityStatusOfHost(s, host))
  const [busy, setBusy] = useState<string | null>(null)
  const hostLabel = !host || host === 'local' ? '本機' : host

  const choices = useMemo<Choice[]>(() => {
    const out: Choice[] = []
    for (const kind of BOT_KINDS) {
      if (!tools[kind]?.installed) continue
      // 沒登入的身份開起來只會停在登入畫面，等於不可行——不列。
      const ids = identitiesOfHost(allIdentities, status).filter((i) => i.kind === kind && status[i.name]?.logged_in !== false)
      if (ids.length === 0) {
        out.push({ key: kind, kind, identity: null, label: kind, title: `在 ${hostLabel} 開一個 ${kind}` })
        continue
      }
      for (const i of ids) {
        const st = status[i.name]
        out.push({
          key: `${kind}:${i.name}`,
          kind,
          identity: i.name,
          label: i.name,
          title: `${kind} · ${i.name}${st?.account ? ` — ${st.account}` : ''}（${hostLabel}）`,
        })
      }
    }
    return out
  }, [tools, allIdentities, status, hostLabel])

  if (choices.length === 0) return null

  const pick = (c: Choice) => {
    setBusy(c.key)
    void addBot(projectId, {
      name: nextName(c.identity ?? c.kind, projectId, bots),
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
    <div className="quick-add" role="group" aria-label="快速新增 Bot">
      {choices.map((c) => (
        <button
          key={c.key}
          type="button"
          className="quick-add-chip"
          disabled={busy !== null}
          title={c.title}
          onClick={() => pick(c)}
        >
          <KindTag kind={c.kind} />
          <span className="quick-add-label">{busy === c.key ? '建立中…' : c.label}</span>
        </button>
      ))}
    </div>
  )
}
