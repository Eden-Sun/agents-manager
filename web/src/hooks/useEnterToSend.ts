import { PHONE_QUERY, useMediaQuery } from './useMediaQuery'

export function useEnterToSend() {
  const phone = useMediaQuery(PHONE_QUERY)
  return {
    enterSends: !phone,
    props: { enterKeyHint: 'enter' as const },
  }
}
