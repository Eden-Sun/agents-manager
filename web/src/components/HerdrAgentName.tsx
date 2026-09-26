import { splitAgentTail } from '../lib/herdrIdentity'

/** 標題列 pane id 後面的 herdr 名（#572）：擠的時候截中間，尾碼留著（chatPanel.css `.pane-agent`）。 */
export function HerdrAgentName({ agent }: { agent: string }) {
  const [head, tail] = splitAgentTail(agent)
  return (
    <span className="pane-agent">
      <span className="pane-agent-head">{head}</span>
      <span className="pane-agent-tail">{tail}</span>
    </span>
  )
}
