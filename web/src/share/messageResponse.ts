export type MessageResponseKind = 'success' | 'not_found' | 'failure'
export type MessagePageState = 'active' | 'gone'
export type MessageResponseDecision = 'apply' | 'discard' | 'expire'

export interface MessageResponseInput {
  requestGen: number
  latestIssuedGen: number
  kind: MessageResponseKind
  appliedGen: number
  settledGen: number
  state: MessagePageState
}

export function decideMessageResponse(input: MessageResponseInput): MessageResponseDecision {
  if (input.requestGen > input.latestIssuedGen) return 'discard'
  if (input.state === 'gone') return 'discard'
  if (input.kind === 'not_found') return 'expire'
  if (input.kind === 'success') return input.requestGen < input.appliedGen ? 'discard' : 'apply'
  return input.requestGen < input.settledGen ? 'discard' : 'apply'
}
