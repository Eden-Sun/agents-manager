import { useStore } from '../store/store'

export function ProjectSettingsPanel({ projectId }: { projectId: string; onOpenSidebar: () => void }) {
  const project = useStore((s) => s.projects.find((p) => p.id === projectId))
  return <div>{project?.label}</div>
}
