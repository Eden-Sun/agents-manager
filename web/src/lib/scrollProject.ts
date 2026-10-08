/**
 * 側欄捲到某個專案並讓它貼齊頂端（2026-09-16 使用者：「menu 也要 scroll 到指定點」；
 * 2026-10-08 使用者：「點選左邊的menu要跟著scroll到指定位置」）。
 * `nearest` 在它只露一半時什麼都不做，看起來像沒捲，所以用 `start`。
 * 點專案標題與 Control+1…9 共用；bot 列的捲動另在 Sidebar（`nearest`，選取的 bot 換了才捲）。
 */
export function scrollProjectIntoView(projectId: string): void {
  if (typeof document === 'undefined') return
  const row = document.querySelector<HTMLElement>(`.project[data-project-id="${CSS.escape(projectId)}"]`)
  row?.scrollIntoView({ block: 'start', behavior: 'smooth' })
}
