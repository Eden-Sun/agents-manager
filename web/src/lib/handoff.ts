import type { Project } from '../api/types'

/**
 * #708：專案已移交給另一台主機的 daemon 管——這顆 daemon 不開、不關、不送 prompt，UI 照實標出來並停用操作。
 * 回傳接手主機的名字；`null`＝這顆 daemon 管。
 */
export function handedOffTo(projects: readonly Project[], projectId: string | null | undefined): string | null {
  const to = projects.find((p) => p.id === projectId)?.handed_off_to
  return to && to.trim() ? to : null
}

export function handedOffReason(host: string): string {
  return `由 ${host} 管理：這裡不送訊息，也不啟動或停止這顆 Bot`
}
