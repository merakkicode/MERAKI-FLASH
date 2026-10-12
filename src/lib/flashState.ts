export type FlashPhase = "idle" | "preparing" | "analyzing" | "splitting" | "formatting"
  | "extracting" | "writing" | "syncing" | "verifying" | "done" | "error" | "cancelled";

export function isTerminalPhase(phase: FlashPhase): boolean {
  return phase === "done" || phase === "error" || phase === "cancelled";
}

export function isFlashBusy(phase: FlashPhase, confirming = false): boolean {
  return confirming || (phase !== "idle" && !isTerminalPhase(phase));
}

export const CANCELLED_MESSAGE = "A gravação foi cancelada. O pendrive foi liberado, mas não está inicializável.";
