import test from "node:test";
import assert from "node:assert/strict";
import { IMAGE_EXTENSIONS, imageFormatLabel, validateImageSelection } from "../src/lib/images.ts";

test("seletor aceita imagens diretas e compactadas no modo Linux", () => {
  assert.deepEqual(IMAGE_EXTENSIONS, ["iso", "img", "raw", "bin", "xz", "gz", "zip"]);
  for (const name of ["disk.iso", "disk.IMG", "disk.raw", "disk.bin", "disk.img.xz", "disk.raw.gz", "disk.ZIP"]) {
    assert.equal(validateImageSelection(name, "linux"), null);
  }
  for (const name of ["notes.txt", "disk.exe", "disk.img.exe"]) {
    assert.notEqual(validateImageSelection(name, "linux"), null);
  }
});

test("modo Windows exige ISO ou IMG sem compressão e revalida ao trocar o sistema", () => {
  for (const name of ["windows.iso", "windows.IMG"]) assert.equal(validateImageSelection(name, "windows"), null);
  for (const name of ["windows.iso.gz", "disk.xz", "disk.zip", "disk.raw", "disk.bin"]) {
    assert.match(validateImageSelection(name, "windows")!, /ISO.*descompactada/);
  }
});

test("badge identifica compressão e imagem RAW", () => {
  assert.equal(imageFormatLabel("disk.img.XZ"), "Comprimido (XZ)");
  assert.equal(imageFormatLabel("disk.raw.gz"), "Comprimido (GZ)");
  assert.equal(imageFormatLabel("disk.zip"), "Comprimido (ZIP)");
  for (const name of ["disk.img", "disk.raw", "disk.bin"]) assert.equal(imageFormatLabel(name), "Imagem RAW");
  assert.equal(imageFormatLabel("windows.iso"), "Imagem ISO");
});
