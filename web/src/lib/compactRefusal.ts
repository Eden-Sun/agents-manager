/** 409 的理由（同 `/login` 的 gate）翻成一句人話；其他錯誤照原文。 */
export function compactRefusal(reason: string): string {
  switch (reason) {
    case 'not_running':
      return '它沒有在跑，沒辦法壓縮'
    case 'turn_in_flight':
      return '它正在回合中，等這一回合做完再壓縮'
    case 'agent_busy':
      return '它正在忙（回合中或停在等你回答），閒下來再壓縮'
    case 'no_pane':
      return '找不到它的終端，沒辦法送'
    default:
      return reason
  }
}
