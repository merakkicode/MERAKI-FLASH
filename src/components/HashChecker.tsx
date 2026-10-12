import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { AlertTriangle, Check, Copy, Fingerprint, LoaderCircle, ShieldCheck, XCircle } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { acceptsHashProgress, compareHashes, type HashAlgorithm, type HashProgress } from "../lib/hash";

interface HashRun { id: string; cancelled: boolean; started: boolean; unlisten?: UnlistenFn }

function stopListening(request: HashRun) {
  const unlisten = request.unlisten;
  request.unlisten = undefined;
  unlisten?.();
}

export function HashChecker({ path, disabled = false }: { path: string; disabled?: boolean }) {
  const [algorithm, setAlgorithm] = useState<HashAlgorithm>("sha256");
  const [digest, setDigest] = useState<string | null>(null);
  const [expected, setExpected] = useState("");
  const [progress, setProgress] = useState<HashProgress | null>(null);
  const [running, setRunning] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const active = useRef<HashRun | null>(null);
  const mounted = useRef(true);
  const copyTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const native = "__TAURI_INTERNALS__" in window;
  const label = algorithm === "sha256" ? "SHA-256" : "MD5";
  const comparison = compareHashes(digest, expected, algorithm);
  const fieldId = `official-hash-${algorithm}`;

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      const request = active.current;
      active.current = null;
      if (request) {
        request.cancelled = true;
        stopListening(request);
        if (request.started) void invoke("cancel_hash", { requestId: request.id }).catch(() => {});
      }
      if (copyTimer.current) clearTimeout(copyTimer.current);
    };
  }, []);

  const calculate = async () => {
    if (active.current || disabled || !native) return;
    const request: HashRun = { id: crypto.randomUUID(), cancelled: false, started: false };
    active.current = request;
    setRunning(true);
    setCancelling(false);
    setDigest(null);
    setCopied(false);
    setProgress(null);
    setNotice(null);
    try {
      const unlisten = await listen<HashProgress>("hash-progress", ({ payload }) => {
        if (mounted.current && active.current === request && !request.cancelled && acceptsHashProgress(request.id, payload)) setProgress(payload);
      });
      request.unlisten = unlisten;
      if (!mounted.current || active.current !== request || request.cancelled) return;
      request.started = true;
      const result = await invoke<string>("compute_image_hash", { path, algorithm, requestId: request.id });
      if (mounted.current && active.current === request && !request.cancelled) setDigest(result);
    } catch (error) {
      if (mounted.current && active.current === request && !request.cancelled) setNotice(String(error));
    } finally {
      stopListening(request);
      if (mounted.current && active.current === request) {
        if (request.cancelled) setNotice("Cálculo de hash cancelado.");
        active.current = null;
        setRunning(false);
        setCancelling(false);
        setProgress(null);
      }
    }
  };

  const cancel = async () => {
    const request = active.current;
    if (!request || request.cancelled) return;
    request.cancelled = true;
    setCancelling(true);
    if (!request.started) return;
    try { await invoke("cancel_hash", { requestId: request.id }); }
    catch (error) {
      if (mounted.current && active.current === request) {
        request.cancelled = false;
        setCancelling(false);
        setNotice(`Não foi possível cancelar: ${String(error)}`);
      }
    }
  };

  const copyDigest = async () => {
    if (!digest) return;
    try {
      await navigator.clipboard.writeText(digest);
      if (!mounted.current) return;
      setCopied(true);
      if (copyTimer.current) clearTimeout(copyTimer.current);
      copyTimer.current = setTimeout(() => setCopied(false), 2000);
    } catch { if (mounted.current) setNotice("Não foi possível copiar. Selecione e copie o hash manualmente."); }
  };

  return (
    <details className="mt-3 rounded-xl border border-[#8cbe80]/20 bg-[#8cbe80]/[0.035] p-3">
      <summary className="cursor-pointer text-xs font-medium text-[#9dd68f] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#8cbe80]/60">
        <Fingerprint className="mr-1.5 inline size-4" />Verificar Hash / Integridade
      </summary>
      <div className="mt-3 space-y-3">
        <p className="text-[11px] leading-4 text-white/45">Confira o arquivo selecionado antes de gravar. Arquivos compactados são verificados sem descompactar.</p>
        <div className="flex flex-wrap items-center gap-2">
          <select aria-label="Algoritmo do hash" value={algorithm} disabled={running || disabled} onChange={(event) => {
            setAlgorithm(event.target.value as HashAlgorithm); setDigest(null); setCopied(false); setNotice(null);
          }} className="rounded-lg border border-white/10 bg-[#101119] px-2 py-2 text-xs text-white/80 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#8cbe80]/60 disabled:opacity-50">
            <option value="sha256">SHA-256</option><option value="md5">MD5 (legado)</option>
          </select>
          <button type="button" disabled={running || disabled || !native} onClick={() => void calculate()}
            className="rounded-lg border border-[#8cbe80]/25 bg-[#8cbe80]/10 px-3 py-2 text-xs text-[#9dd68f] transition hover:bg-[#8cbe80]/20 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#8cbe80]/60 disabled:cursor-not-allowed disabled:opacity-45">
            {digest ? "Recalcular" : `Calcular ${label}`}
          </button>
          {running && <button type="button" disabled={cancelling} onClick={() => void cancel()} className="inline-flex items-center gap-1 rounded-lg border border-rose-500/25 bg-rose-500/10 px-2 py-2 text-xs text-rose-300 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-rose-400/60 disabled:opacity-50">
            {cancelling ? <LoaderCircle className="size-3 animate-spin" /> : <XCircle className="size-3" />}{cancelling ? "Cancelando…" : "Cancelar cálculo"}
          </button>}
        </div>
        {!native && <p className="text-[11px] text-amber-200/80">O cálculo fica disponível no aplicativo desktop.</p>}
        {algorithm === "md5" && <p className="text-[11px] leading-4 text-white/45">MD5 é legado. Prefira SHA-256 quando a distribuição o fornecer.</p>}
        {running && <div>
          <p role="status" className="mb-2 text-[11px] text-white/65">{cancelling ? "Aguardando o encerramento da leitura…" : `Calculando ${label}… ${Math.floor(progress?.percentage ?? 0)}% (${((progress?.bytesPerSecond ?? 0) / 1024 / 1024).toFixed(1)} MiB/s)`}</p>
          <div role="progressbar" aria-label={`Cálculo de ${label}`} aria-valuenow={progress?.percentage ?? 0} aria-valuemin={0} aria-valuemax={100} className="h-1.5 overflow-hidden rounded-full bg-white/10">
            <div className="h-full bg-[#8cbe80] transition-[width]" style={{ width: `${Math.min(100, Math.max(0, progress?.percentage ?? 0))}%` }} />
          </div>
        </div>}
        {digest && <div className="rounded-lg border border-white/10 bg-black/20 p-2.5">
          <div className="mb-2 flex items-center justify-between gap-2 text-[11px] text-white/50"><span>{label} calculado</span>
            <button type="button" onClick={() => void copyDigest()} className="inline-flex items-center gap-1 rounded text-[#9dd68f] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#8cbe80]/60" aria-label="Copiar hash">
              {copied ? <Check className="size-3" /> : <Copy className="size-3" />}{copied ? "Copiado!" : "Copiar"}
            </button>
          </div><code className="block select-all break-all font-mono text-[11px] leading-5 text-white/80">{digest}</code>
        </div>}
        <div>
          <label htmlFor={fieldId} className="mb-1.5 block text-[11px] font-medium text-white/65">Comparar com o oficial</label>
          <textarea id={fieldId} value={expected} onChange={(event) => setExpected(event.target.value)} rows={2} maxLength={8192}
            placeholder="Cole o hash oficial fornecido pela distribuição..." spellCheck={false} autoCapitalize="none"
            aria-invalid={comparison === "invalid" || comparison === "mismatch"} aria-describedby={`${fieldId}-feedback`}
            className="w-full resize-y rounded-lg border border-white/10 bg-black/20 p-2 font-mono text-[11px] leading-5 text-white/80 placeholder:text-white/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#8cbe80]/60" />
          <div id={`${fieldId}-feedback`} aria-live="polite" className="mt-2 text-[11px] leading-4">
            {comparison === "match" && <p className="flex items-start gap-1.5 rounded-lg bg-[#8cbe80]/10 p-2 text-[#9dd68f]"><ShieldCheck className="size-4 shrink-0" />O hash coincide com o valor informado. Confirme que ele veio da fonte oficial.</p>}
            {comparison === "mismatch" && <p className="flex items-start gap-1.5 rounded-lg bg-rose-500/10 p-2 text-rose-300"><AlertTriangle className="size-4 shrink-0" />ATENÇÃO: O hash NÃO coincide! O arquivo pode estar corrompido, incompleto ou ser de outra versão.</p>}
            {comparison === "invalid" && <p className="text-amber-200/90">Cole um único hash {label} válido, com {algorithm === "sha256" ? 64 : 32} caracteres hexadecimais. Linhas SHA256SUMS/MD5SUMS são aceitas.</p>}
            {comparison === "idle" && expected.trim() && !digest && <p className="text-white/45">Calcule o hash para comparar com o valor informado.</p>}
          </div>
        </div>
        {notice && <p role="status" className="text-[11px] leading-4 text-amber-200/90">{notice}</p>}
      </div>
    </details>
  );
}
