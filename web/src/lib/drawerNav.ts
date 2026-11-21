export const DRAWER_NAV = '.bot-row, .project-head'
export const DRAWER_STAY =
  '.bot-kids-toggle, .bot-actions, .bot-name-btn, .bot-name-input, .project-fold, .project-head-actions, .bot-cluster-chip-wrap'

export function drawerShouldClose(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false
  if (!target.closest(DRAWER_NAV)) return false
  return !target.closest(DRAWER_STAY)
}
