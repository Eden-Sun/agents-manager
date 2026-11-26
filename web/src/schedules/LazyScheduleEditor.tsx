import { lazy, Suspense, type ComponentProps } from 'react'
import type { ScheduleEditor as Editor } from './ScheduleEditor'

// 排程編輯器只在打開時才要，拆成另一個 chunk（首頁不下載）。
const Lazy = lazy(() => import('./ScheduleEditor').then((m) => ({ default: m.ScheduleEditor })))

export function ScheduleEditorLazy(props: ComponentProps<typeof Editor>) {
  return (
    <Suspense fallback={null}>
      <Lazy {...props} />
    </Suspense>
  )
}
