export function shortTimeZoneList(): { restore: () => void } {
  const intl = Intl as unknown as { supportedValuesOf?: (key: string) => string[] }
  const real = intl.supportedValuesOf
  intl.supportedValuesOf = (key: string) => (key === 'timeZone' ? ['Asia/Taipei', 'Asia/Tokyo', 'America/New_York', 'Europe/London'] : real?.(key) ?? [])
  return {
    restore: () => {
      intl.supportedValuesOf = real
    },
  }
}
