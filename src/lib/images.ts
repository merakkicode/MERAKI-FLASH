export const IMAGE_EXTENSIONS = ["iso", "img", "raw", "bin", "xz", "gz", "zip"];

function extension(name: string): string {
  return name.split(".").pop()?.toLowerCase() ?? "";
}

export function validateImageSelection(name: string, system: "windows" | "linux" | null): string | null {
  const ext = extension(name);
  if (!IMAGE_EXTENSIONS.includes(ext)) {
    return "Selecione uma imagem (.iso, .img, .raw, .bin, .xz, .gz, .zip).";
  }
  if (system === "windows" && ext !== "iso" && ext !== "img") {
    return "Para Windows, selecione uma ISO (.iso ou .img) descompactada. Imagens compactadas são aceitas no modo Linux.";
  }
  return null;
}

export function imageFormatLabel(name: string): string {
  const ext = extension(name);
  if (["xz", "gz", "zip"].includes(ext)) return `Comprimido (${ext.toUpperCase()})`;
  if (["img", "raw", "bin"].includes(ext)) return "Imagem RAW";
  return ext === "iso" ? "Imagem ISO" : "Formato não suportado";
}
