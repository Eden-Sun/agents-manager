import { draftTooLargeText, useDraftNotice } from '../store/draftNotice'
import './draftTooLargeNote.css'

export function DraftTooLargeNote({ draftKey }: { draftKey: string }) {
  const max = useDraftNotice((s) => s.rejected[draftKey])
  if (max === undefined) return null
  return (
    <div className="draft-too-large" role="status">
      {draftTooLargeText(max)}
    </div>
  )
}
