import { AlertTriangle, RotateCcw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { canFormatDevice, normalizeVolumeLabel, validateVolumeLabel, type FormatDevice } from "../lib/format";

export function FormatModal({ device, onClose, onConfirm }: {
  device: FormatDevice; onClose: () => void; onConfirm: (volumeLabel: string) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [label, setLabel] = useState("PENDRIVE");
  const error = validateVolumeLabel(label);
  useEffect(() => {
    const element = dialog.current;
    element?.showModal();
    return () => { if (element?.open) element.close(); };
  }, []);

  return <dialog ref={dialog} aria-labelledby="format-title" aria-describedby="format-warning"
    onCancel={(event) => { event.preventDefault(); onClose(); }}
    className="m-auto w-[calc(100%-2rem)] max-w-lg rounded-2xl border border-white/10 bg-[#101119] p-6 text-white shadow-2xl backdrop:bg-black/75">
    <form onSubmit={(event) => { event.preventDefault(); if (!error && canFormatDevice(device)) onConfirm(normalizeVolumeLabel(label)); }}>
      <h2 id="format-title" className="flex items-center gap-2 text-lg font-semibold"><RotateCcw className="size-5 shrink-0 text-[#9dd68f]" />Restaurar pendrive para uso normal</h2>
      <div className="my-4 rounded-xl border border-white/10 bg-white/[0.03] p-3 text-sm">
        <p className="font-semibold">{device.name}</p>
        <p className="mt-1 text-xs text-white/60">{(device.totalBytes / 1024 ** 3).toLocaleString("pt-BR", { maximumFractionDigits: 1 })} GiB</p>
        <p className="mt-1 break-all font-mono text-xs text-white/60">{device.devicePath}</p>
      </div>
      <label htmlFor="format-label" className="mb-2 block text-xs font-medium text-white/75">Nome do volume (Rótulo)</label>
      <input id="format-label" value={label} onChange={(event) => setLabel(normalizeVolumeLabel(event.target.value))}
        maxLength={11} spellCheck={false} autoComplete="off" aria-invalid={Boolean(error)} aria-describedby="format-label-error"
        className="w-full rounded-xl border border-white/10 bg-black/20 px-3 py-2.5 text-sm uppercase focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#8cbe80]/60" />
      <p id="format-label-error" aria-live="polite" className="mt-2 text-xs text-rose-300">{error}</p>
      <div id="format-warning" className="my-4 flex gap-2 rounded-xl border border-amber-400/20 bg-amber-400/10 p-3 text-xs leading-5 text-amber-200">
        <AlertTriangle className="mt-0.5 size-4 shrink-0" /><p>Todos os dados e partições serão apagados. Uma única partição FAT32 usará a capacidade disponível, com pequenas reservas para alinhamento e metadados.</p>
      </div>
      <p className="mb-5 text-[11px] leading-4 text-white/45">Formatação rápida: reorganiza o pendrive para uso comum. FAT32 aceita arquivos de até 4 GiB menos 1 byte.</p>
      <div className="flex flex-wrap justify-end gap-2">
        <button type="button" autoFocus onClick={onClose} className="rounded-xl border border-white/10 px-4 py-2.5 text-sm text-white/70 hover:bg-white/5 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#8cbe80]/60">Cancelar</button>
        <button type="submit" disabled={Boolean(error) || !canFormatDevice(device)} className="rounded-xl border border-amber-400/30 bg-amber-400/15 px-4 py-2.5 text-sm font-semibold text-amber-200 hover:bg-amber-400/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-amber-400/60 disabled:cursor-not-allowed disabled:opacity-40">Limpar e Formatar</button>
      </div>
    </form>
  </dialog>;
}
