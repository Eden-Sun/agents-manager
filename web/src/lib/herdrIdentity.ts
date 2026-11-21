export interface HerdrIdentity {
  pane: string
  agent: string
  text: string
  title: string
}

export function herdrIdentity(
  run: { pane_id?: string | null } | null | undefined,
  agentName: string | null | undefined,
): HerdrIdentity | null {
  if (!run) return null
  const pane = run.pane_id?.trim() || ''
  const agent = agentName?.trim() || ''
  if (!pane && !agent) return null
  const text = [pane, agent].filter(Boolean).join(' · ')
  const title = [agent ? `herdr agent ${agent}` : '', pane ? `pane ${pane}` : ''].filter(Boolean).join('・')
  return { pane, agent, text, title }
}

export const AGENT_TAIL = 6

export function splitAgentTail(agent: string): [head: string, tail: string] {
  if (agent.length <= AGENT_TAIL) return ['', agent]
  return [agent.slice(0, -AGENT_TAIL), agent.slice(-AGENT_TAIL)]
}
