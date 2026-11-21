export function scrollProjectIntoView(projectId: string): void {
  if (typeof document === 'undefined') return
  const row = document.querySelector<HTMLElement>(`.project[data-project-id="${CSS.escape(projectId)}"]`)
  row?.scrollIntoView({ block: 'start', behavior: 'smooth' })
}
