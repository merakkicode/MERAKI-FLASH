import assert from "node:assert/strict";
import test from "node:test";
import { acceptsHashProgress, compareHashes, parseHashInput } from "../src/lib/hash.ts";

const sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const md5 = "d41d8cd98f00b204e9800998ecf8427e";

test("normaliza SHA-256 e MD5 com maiúsculas e espaços", () => {
  for (const hash of [sha, md5]) assert.equal(parseHashInput(`  ${hash.toUpperCase()}\n`), hash);
});
test("extrai linhas sha256sum/md5sum e BSD com nomes de arquivo", () => {
  assert.equal(parseHashInput(`${sha}  ubuntu-24.04-desktop-amd64.iso`), sha);
  assert.equal(parseHashInput(`${sha} *raspios.img.xz`), sha);
  assert.equal(parseHashInput(`SHA256 (ubuntu.iso) = ${sha.toUpperCase()}`), sha);
  assert.equal(parseHashInput(`MD5 (windows.iso) = ${md5}`), md5);
});
test("rejeita entradas inválidas, comprimentos errados e listas ambíguas", () => {
  for (const input of ["", "   ", "not a hash", "g".repeat(64), "a".repeat(63), "a".repeat(65), "a".repeat(33), `z${sha}`, `${sha}z`, `${sha}\n${"a".repeat(64)}`, `${sha}\n${md5}`]) {
    assert.equal(parseHashInput(input), null, input);
  }
  assert.equal(parseHashInput(md5, "sha256"), null);
  assert.equal(parseHashInput(sha, "md5"), null);
});
test("compara sem diferenciar maiúsculas, com os quatro estados", () => {
  assert.equal(compareHashes(sha, "  "), "idle");
  assert.equal(compareHashes(null, sha), "idle");
  assert.equal(compareHashes(sha, "texto inválido"), "invalid");
  assert.equal(compareHashes(sha.toUpperCase(), `SHA256 (image.iso) = ${sha}`), "match");
  assert.equal(compareHashes(sha, "a".repeat(64)), "mismatch");
  assert.equal(compareHashes(md5, md5.toUpperCase(), "md5"), "match");
  assert.equal(compareHashes(sha, md5), "invalid");
});
test("eventos de um cálculo anterior não atualizam a imagem selecionada", () => {
  const progress = { requestId: "old", percentage: 75, bytesProcessed: 3, totalBytes: 4, bytesPerSecond: 1 };
  assert.equal(acceptsHashProgress("new", progress), false);
  assert.equal(acceptsHashProgress(null, progress), false);
  assert.equal(acceptsHashProgress("old", progress), true);
});
