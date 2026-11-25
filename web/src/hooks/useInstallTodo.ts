import { claudeInstallPlan, codexInstallPlan, type ClaudeInstallPlan, type CodexInstallPlan } from '../lib/updateBatch'
import { updateRange } from '../lib/updateRange'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import type { CliUpdate } from '../store/cliUpdate'

export interface InstallTodo {
  claude: { target: string; hosts: ClaudeInstallPlan['hosts']; toInstall: string[] } | null
  codex: { host: string; target: string; notice: string; installCount: number } | null
  active: CliUpdate[]
  count: number
}

export function useInstallTodo(): InstallTodo {
  const key = useStore((s) => {
    const active = s.cliUpdates.filter((item) => item.kind === 'claude' || item.kind === 'codex')
    const claude = claudeInstallPlan(s.upstreamUpdates.claude)
    const codex = codexInstallPlan(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null, (b) => projectHostName(s, b.project_id), s.upstreamUpdates.codex)
    return JSON.stringify({ active, claude, codex })
  })
  const { active, claude, codex } = JSON.parse(key) as { active: CliUpdate[]; claude: ClaudeInstallPlan | null; codex: CodexInstallPlan | null }
  const busyHosts = (kind: string) => new Set(active.filter((item) => item.kind === kind).map((item) => item.host))
  const claudeBusy = busyHosts('claude')
  const claudeTodo = claude
    ? { target: claude.target, hosts: claude.hosts, toInstall: claude.installHosts.filter((host) => !claudeBusy.has(host)) }
    : null
  const codexTodo = codex && !busyHosts('codex').has(codex.host)
    ? { host: codex.host, target: updateRange('codex', codex.notice, null).to ?? '', notice: codex.notice, installCount: codex.installCount }
    : null
  const count = (claudeTodo?.toInstall.length ?? 0) + (codexTodo ? 1 : 0)
  return { claude: claudeTodo, codex: codexTodo, active, count }
}
