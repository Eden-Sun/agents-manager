import { rawTransport } from './index'

export interface TimeZoneSettingsValue {
  time_zone: string
  utc_offset: string
}

export async function getTimeZone(): Promise<TimeZoneSettingsValue> {
  return (await rawTransport.request('GET', '/settings/time-zone')) as TimeZoneSettingsValue
}

export async function setTimeZone(zone: string): Promise<TimeZoneSettingsValue> {
  return (await rawTransport.request('PUT', '/settings/time-zone', { time_zone: zone })) as TimeZoneSettingsValue
}
