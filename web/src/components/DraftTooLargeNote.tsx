import { draftTooLargeText, useDraftNotice } from '../store/draftNotice'
import './draftTooLargeNote.css'

/** 輸入框草稿被 daemon 拒收（太大）時，輸入列上方的一行說明（#1656）；縮短送成功後自己消失。 */
export function DraftTooLargeNote({ draftKey }: { draftKey: string }) {
  const max = useDraftNotice((s) => s.rejected[draftKey])
  if (max === undefined) return null
  return (
    <div className="draft-too-large" role="status">
      {draftTooLargeText(max)}
    </div>
  )
}
