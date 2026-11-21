import { parseJevNotice } from '../lib/jevNotice'
import { JevLabel } from './JevLabel'

export function JevNoticeText({ text }: { text: string }) {
  const n = parseJevNotice(text)
  if (!n) return <>{text}</>
  return (
    <>
      <JevLabel prob={n.prob} note />
      {n.rest ? <> {n.rest}</> : null}
    </>
  )
}
