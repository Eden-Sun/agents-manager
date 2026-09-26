/**
 * 把網頁上的一顆 bot 對到終端（#572）：暱稱（`bots.name`）與 herdr agent 名（`<專案 slug>-<id 尾 6 碼>`）
 * 本來就無關（SPEC §1），使用者在 herdr 看到的是後者與 pane id。
 *
 * 只在有 active run 時給；沒有 run 就只剩暱稱（回 `null`）。
 */
export interface HerdrIdentity {
  pane: string
  agent: string
  /** 一行版，例如 `w16T:p2 · pt-hub-sytk6j`。 */
  text: string
  /** 滑過去的完整說明。 */
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

/** 標題列擠的時候從中間截：尾巴 6 碼（herdr 名的 id 尾碼、子 agent 的字尾）才分得出是哪一顆，不能被截掉。 */
export const AGENT_TAIL = 6

export function splitAgentTail(agent: string): [head: string, tail: string] {
  if (agent.length <= AGENT_TAIL) return ['', agent]
  return [agent.slice(0, -AGENT_TAIL), agent.slice(-AGENT_TAIL)]
}
