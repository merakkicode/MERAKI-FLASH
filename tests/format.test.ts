import assert from "node:assert/strict";
import test from "node:test";
import { canFormatDevice, normalizeVolumeLabel, validateVolumeLabel } from "../src/lib/format.ts";

test("normaliza letras ASCII e aceita rótulos de 1 a 11 caracteres", () => {
  assert.equal(normalizeVolumeLabel("pendrive123"), "PENDRIVE123");
  for (const value of ["A", "PENDRIVE", "12345678901", "usb123"]) assert.equal(validateVolumeLabel(value), null);
});
test("rejeita rótulos vazios, longos, Unicode, espaços e símbolos", () => {
  for (const value of ["", "123456789012", "USB DISK", "A ", "Á", "ß", "a/b", "a\\b", "a.b", "a-b", "a\0b", 'A:B*?"<>|+[]', "A\n"]) {
    assert.notEqual(validateVolumeLabel(normalizeVolumeLabel(value)), null, value);
  }
});
test("formatação depende de destino gravável e operação livre, sem exigir ISO", () => {
  const device = { id: "usb", name: "Pendrive", devicePath: "disk-test", totalBytes: 1024 ** 3, readOnly: false };
  assert.equal(canFormatDevice(device), true);
  assert.equal(canFormatDevice(null), false);
  assert.equal(canFormatDevice({ ...device, readOnly: true }), false);
  assert.equal(canFormatDevice({ ...device, totalBytes: 0 }), false);
  assert.equal(canFormatDevice(device, true), false);
});
