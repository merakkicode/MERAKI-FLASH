export type HashAlgorithm = "sha256" | "md5";
export type HashComparison = "idle" | "match" | "mismatch" | "invalid";

/** Aceita digest isolado, sha256sum/md5sum e o formato BSD. Rejeita ambiguidade. */
export function parseHashInput(input: string, algorithm?: HashAlgorithm): string | null {
  const matches = input.trim().match(/(?<![a-z0-9])[a-f0-9]{64}(?![a-z0-9])|(?<![a-z0-9])[a-f0-9]{32}(?![a-z0-9])/gi);
  const hashes = [...new Set(matches?.map((hash) => hash.toLowerCase()) ?? [])];
  if (hashes.length !== 1) return null;
  const hash = hashes[0];
  if (algorithm && hash.length !== (algorithm === "sha256" ? 64 : 32)) return null;
  return hash;
}

export function compareHashes(calculated: string | null, input: string, algorithm?: HashAlgorithm): HashComparison {
  if (!input.trim()) return "idle";
  const expected = parseHashInput(input, algorithm);
  if (!expected) return "invalid";
  if (!calculated) return "idle";
  const actual = parseHashInput(calculated, algorithm);
  if (!actual || actual.length !== expected.length) return "invalid";
  return actual === expected ? "match" : "mismatch";
}

export interface HashProgress {
  requestId: string;
  percentage: number;
  bytesProcessed: number;
  totalBytes: number;
  bytesPerSecond: number;
}

export function acceptsHashProgress(activeId: string | null, progress: HashProgress): boolean {
  return activeId !== null && activeId === progress.requestId;
}
