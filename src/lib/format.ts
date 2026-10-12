export interface FormatDevice {
  id: string;
  name: string;
  devicePath: string;
  totalBytes: number;
  readOnly: boolean;
}

export function normalizeVolumeLabel(value: string): string {
  return value.replace(/[a-z]/g, (letter) => letter.toUpperCase());
}

export function validateVolumeLabel(value: string): string | null {
  return value.length >= 1 && value.length <= 11 && !/[^a-z0-9]/i.test(value)
    ? null : "Use de 1 a 11 letras de A a Z ou números, sem espaços ou símbolos.";
}

export function canFormatDevice(device: FormatDevice | null, busy = false): boolean {
  return Boolean(device && device.id && device.totalBytes > 0 && !device.readOnly && !busy);
}
