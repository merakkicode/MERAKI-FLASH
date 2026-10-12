use super::Reporter;
use super::cancellation::{CancellableIo, finish_io};
#[cfg(test)]
use super::disk_layout::format_fat32;
use super::disk_layout::{ALIGNMENT_BYTES, create_partition_table, format_fat32_cancellable};
use super::platform;
use super::protocol::{FlashPhase, FlashProgress};
use super::sector_io::SectorIo;
use fatfs::{FileSystem, FsOptions, ReadWriteSeek};
use fscommon::{BufStream, StreamSlice};
use isomage::{TreeNode, cat_node};
#[cfg(test)]
use mbrman::{BOOT_ACTIVE, MBR};
use meraki_flash_core::UsbDevice;
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::{Builder, TempDir};

const COPY_BUFFER_SIZE: usize = 4 * 1024 * 1024;
const FAT32_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024 - 1;
const SWM_PART_BYTES: u64 = 3_800_000_000;
const MIN_FREE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UNATTEND_BYTES: usize = 16 * 1024;
const WIM_WORKSPACE_RESERVE_BYTES: u64 = 512 * 1024 * 1024;

pub struct WindowsImage {
    source: File,
    root: TreeNode,
    split: Option<SplitArtifacts>,
    unattend_xml: Option<String>,
    tracker: ProgressTracker,
}

struct SplitArtifacts {
    _temporary_directory: TempDir,
    source_path: String,
    parts: Vec<PathBuf>,
}

pub fn prepare(
    mut source: File,
    iso_path: &Path,
    device: &UsbDevice,
    unattend_xml: Option<String>,
    operation_id: &str,
    reporter: &mut Reporter,
) -> Result<WindowsImage, String> {
    send_status(
        reporter,
        operation_id,
        FlashPhase::Analyzing,
        0.0,
        "Analisando a estrutura ISO/UDF do Windows…",
    );

    reporter.cancellation.check()?;
    let iso_size = source
        .metadata()
        .map_err(|error| format!("não foi possível consultar a ISO: {error}"))?
        .len();
    let root =
        parse_windows_filesystem(&mut CancellableIo::new(&mut source, &reporter.cancellation))
            .map_err(|error| format!("a imagem não contém uma mídia Windows legível: {error}"))?;
    reporter.cancellation.check()?;
    let summary = validate_windows_tree(&root, iso_size)?;

    if !has_uefi_loader(&root) {
        return Err("a ISO não contém um carregador UEFI em EFI/BOOT (boot*.efi)".to_owned());
    }

    let wim_to_split = install_image_to_split(&root)?;
    let unattend_bytes = unattend_xml.as_ref().map_or(0, |xml| xml.len() as u64);
    let total_work = summary
        .total_file_bytes
        .saturating_add(
            wim_to_split
                .map(|node| estimated_split_bytes(node.size, is_esd(node)).saturating_mul(2))
                .unwrap_or(0),
        )
        .saturating_add(unattend_bytes);
    let mut tracker = ProgressTracker::new(total_work.max(1));

    let split = if let Some(wim) = wim_to_split {
        Some(prepare_split_wim(
            &mut source,
            wim,
            iso_path,
            operation_id,
            reporter,
            &mut tracker,
        )?)
    } else {
        None
    };

    let copied_payload_bytes = if let (Some(wim), Some(split)) = (wim_to_split, split.as_ref()) {
        let split_bytes = split.parts.iter().try_fold(0u64, |total, path| {
            let size = path
                .metadata()
                .map_err(|error| format!("não foi possível consultar {}: {error}", path.display()))?
                .len();
            total
                .checked_add(size)
                .ok_or_else(|| "o tamanho da mídia excede o limite suportado".to_owned())
        })?;
        summary
            .total_file_bytes
            .saturating_sub(wim.size)
            .saturating_add(split_bytes)
    } else {
        summary.total_file_bytes
    }
    .saturating_add(unattend_bytes);

    // A conversão de ESD altera o tamanho final; use os bytes reais dos SWMs
    // tanto para a capacidade do destino quanto para o trabalho restante.
    tracker.total_bytes = tracker
        .completed_bytes
        .saturating_add(copied_payload_bytes)
        .max(1);

    let usable_bytes = device.total_bytes.saturating_sub(2 * ALIGNMENT_BYTES);
    let safety_margin = (copied_payload_bytes / 100).max(MIN_FREE_MARGIN_BYTES);
    if copied_payload_bytes.saturating_add(safety_margin) > usable_bytes {
        return Err(format!(
            "o conteúdo da ISO precisa de aproximadamente {} bytes, mas o pendrive oferece {} bytes úteis",
            copied_payload_bytes.saturating_add(safety_margin),
            usable_bytes
        ));
    }

    send_status(
        reporter,
        operation_id,
        FlashPhase::Analyzing,
        tracker.percentage(),
        &format!("ISO Windows validada: {}", iso_path.display()),
    );

    Ok(WindowsImage {
        source,
        root,
        split,
        unattend_xml,
        tracker,
    })
}

pub fn write(
    mut image: WindowsImage,
    device: &UsbDevice,
    operation_id: &str,
    reporter: &mut Reporter,
) -> Result<(), String> {
    send_status(
        reporter,
        operation_id,
        FlashPhase::Preparing,
        image.tracker.percentage(),
        "Desmontando e bloqueando o dispositivo…",
    );
    reporter.cancellation.check()?;
    let mut prepared = platform::prepare_device(device)?;
    let result = (|| {
        let sector_size = platform::logical_sector_size(&prepared.file)?;

        let mut target = SectorIo::new(&mut prepared.file, sector_size, device.total_bytes)
            .map_err(|error| format!("geometria do dispositivo inválida: {error}"))?;
        send_status(
            reporter,
            operation_id,
            FlashPhase::Formatting,
            image.tracker.percentage(),
            "Criando uma partição MBR/FAT32 compatível com UEFI…",
        );
        let (partition_start, partition_end) = create_partition_table(
            &mut CancellableIo::new(&mut target, &reporter.cancellation),
            device.total_bytes,
            sector_size,
        )?;
        format_fat32_cancellable(
            &mut target,
            partition_start,
            partition_end,
            sector_size,
            &reporter.cancellation,
            *b"MERAKI     ",
        )?;

        let partition = StreamSlice::new(&mut target, partition_start, partition_end)
            .map_err(|error| format!("não foi possível abrir a partição FAT32: {error}"))?;
        let filesystem = FileSystem::new(BufStream::new(partition), FsOptions::new())
            .map_err(|error| format!("não foi possível abrir o FAT32 recém-criado: {error}"))?;
        let copied: Result<(), String> = (|| {
            let root_dir = filesystem.root_dir();
            copy_directory(
                &mut image.source,
                &image.root,
                &root_dir,
                "",
                image.split.as_ref(),
                operation_id,
                reporter,
                &mut image.tracker,
            )?;
            reporter.cancellation.check()?;
            if let Some(xml) = image.unattend_xml.as_deref() {
                send_status(
                    reporter,
                    operation_id,
                    FlashPhase::Extracting,
                    image.tracker.percentage(),
                    "Adicionando autounattend.xml à raiz do pendrive…",
                );
                write_unattend_file(&root_dir, xml)?;
                image.tracker.add(
                    xml.len() as u64,
                    FlashPhase::Extracting,
                    reporter,
                    operation_id,
                    Some("autounattend.xml criado com sucesso".to_owned()),
                );
            }
            Ok(())
        })();
        filesystem
            .unmount()
            .map_err(|error| format!("não foi possível finalizar o FAT32: {error}"))?;
        copied?;
        reporter.cancellation.check()?;

        send_status(
            reporter,
            operation_id,
            FlashPhase::Syncing,
            100.0,
            "Sincronizando a tabela de partições e os arquivos…",
        );
        let _ = target
            .finish()
            .map_err(|error| format!("falha ao finalizar os setores: {error}"))?;
        Ok(())
    })();
    // Buffers FAT já foram drenados antes de sync_all e da liberação dos locks.
    let result = finish_io(result, prepared.file.sync_all());
    drop(prepared);
    drop(image);
    result
}

pub fn validate_unattend_xml(content: Option<&str>) -> Result<Option<String>, String> {
    let Some(content) = content else {
        return Ok(None);
    };
    let content = content.trim();
    if content.is_empty() {
        return Err("o conteúdo de autounattend.xml está vazio".to_owned());
    }
    if content.len() > MAX_UNATTEND_BYTES {
        return Err(format!(
            "autounattend.xml excede o limite de {} KiB",
            MAX_UNATTEND_BYTES / 1024
        ));
    }
    if content.contains('\0') {
        return Err("autounattend.xml contém um caractere NUL inválido".to_owned());
    }

    let mut reader = Reader::from_str(content);
    let mut depth = 0usize;
    let mut root_seen = false;
    let mut root_closed = false;
    let mut settings_seen = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                if depth == 0 {
                    if root_seen || element.name().as_ref() != b"unattend" {
                        return Err(
                            "autounattend.xml deve ter um único elemento raiz <unattend>"
                                .to_owned(),
                        );
                    }
                    root_seen = true;
                } else if root_closed {
                    return Err("autounattend.xml contém conteúdo após a raiz".to_owned());
                } else if depth == 1 && element.name().as_ref() == b"settings" {
                    settings_seen = true;
                }
                depth += 1;
            }
            Ok(Event::Empty(element)) => {
                if depth == 0 {
                    return Err(format!(
                        "autounattend.xml não pode usar uma raiz vazia <{} />",
                        String::from_utf8_lossy(element.name().as_ref())
                    ));
                }
                if depth == 1 && element.name().as_ref() == b"settings" {
                    settings_seen = true;
                }
            }
            Ok(Event::End(_)) => {
                if depth == 0 {
                    return Err("autounattend.xml contém uma tag de fechamento inválida".to_owned());
                }
                depth -= 1;
                if depth == 0 {
                    root_closed = true;
                }
            }
            Ok(Event::DocType(_)) => {
                return Err("autounattend.xml não pode conter DTD/DOCTYPE".to_owned());
            }
            Ok(Event::Text(text)) if depth == 0 => {
                let bytes: &[u8] = text.as_ref();
                if bytes.iter().any(|byte| !byte.is_ascii_whitespace()) {
                    return Err("autounattend.xml contém texto fora da raiz".to_owned());
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("autounattend.xml é inválido: {error}")),
        }
    }

    if !root_seen || !root_closed || depth != 0 || !settings_seen {
        return Err(
            "autounattend.xml precisa conter <unattend> e ao menos uma seção <settings>".to_owned(),
        );
    }
    Ok(Some(content.to_owned()))
}

fn parse_windows_filesystem(source: &mut (impl Read + Seek)) -> Result<TreeNode, String> {
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    match isomage::udf::parse_udf(source) {
        Ok(root) => Ok(root),
        Err(udf_error) => {
            source
                .seek(SeekFrom::Start(0))
                .map_err(|error| error.to_string())?;
            isomage::iso9660::parse_iso9660(source)
                .map_err(|iso_error| format!("UDF: {udf_error}; ISO9660: {iso_error}"))
        }
    }
}

#[derive(Debug)]
struct TreeSummary {
    total_file_bytes: u64,
}

fn validate_windows_tree(root: &TreeNode, iso_size: u64) -> Result<TreeSummary, String> {
    fn visit(
        node: &TreeNode,
        components: &mut Vec<String>,
        iso_size: u64,
        total: &mut u64,
    ) -> Result<(), String> {
        if node.name != "/" {
            validate_fat_name(&node.name)?;
            components.push(node.name.clone());
        }

        if node.is_directory {
            let mut names = HashSet::new();
            for child in &node.children {
                if !names.insert(child.name.to_lowercase()) {
                    return Err(format!(
                        "a ISO contém nomes incompatíveis com FAT32 no mesmo diretório: {}",
                        child.name
                    ));
                }
                visit(child, components, iso_size, total)?;
            }
        } else {
            let relative = components.join("/");
            if node.size > FAT32_MAX_FILE_BYTES
                && !relative.eq_ignore_ascii_case("sources/install.wim")
                && !relative.eq_ignore_ascii_case("sources/install.esd")
            {
                return Err(format!(
                    "o arquivo {relative} excede o limite de 4 GiB do FAT32"
                ));
            }
            if node.size > 0 {
                let location = node.file_location.ok_or_else(|| {
                    format!("a ISO não informa onde os dados de {relative} estão armazenados")
                })?;
                let length = node
                    .file_length
                    .ok_or_else(|| format!("a ISO não informa o tamanho físico de {relative}"))?;
                if length != node.size
                    || location
                        .checked_add(length)
                        .is_none_or(|end| end > iso_size)
                {
                    return Err(format!("a extensão de dados de {relative} é inválida"));
                }
            }
            *total = total
                .checked_add(node.size)
                .ok_or_else(|| "o tamanho da ISO excede o limite suportado".to_owned())?;
        }

        if node.name != "/" {
            components.pop();
        }
        Ok(())
    }

    let mut total = 0;
    visit(root, &mut Vec::new(), iso_size, &mut total)?;
    Ok(TreeSummary {
        total_file_bytes: total,
    })
}

fn validate_fat_name(name: &str) -> Result<(), String> {
    let utf16_len = name.encode_utf16().count();
    let invalid = name.is_empty()
        || matches!(name, "." | "..")
        || utf16_len > 255
        || name.ends_with([' ', '.'])
        || name
            .chars()
            .any(|character| character < '\u{20}' || "<>:\"/\\|?*\0".contains(character));
    if invalid {
        return Err(format!(
            "nome incompatível com FAT32 encontrado na ISO: {name:?}"
        ));
    }

    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err(format!(
            "nome reservado do Windows encontrado na ISO: {name}"
        ));
    }
    Ok(())
}

fn has_uefi_loader(root: &TreeNode) -> bool {
    find_path_case_insensitive(root, &["efi", "boot"]).is_some_and(|directory| {
        directory.is_directory
            && directory.children.iter().any(|node| {
                let name = node.name.to_ascii_lowercase();
                !node.is_directory && name.starts_with("boot") && name.ends_with(".efi")
            })
    })
}

fn find_path_case_insensitive<'a>(root: &'a TreeNode, components: &[&str]) -> Option<&'a TreeNode> {
    components.iter().try_fold(root, |current, component| {
        current
            .children
            .iter()
            .find(|child| child.name.eq_ignore_ascii_case(component))
    })
}

fn is_esd(node: &TreeNode) -> bool {
    node.name.eq_ignore_ascii_case("install.esd")
}

fn install_image_to_split(root: &TreeNode) -> Result<Option<&TreeNode>, String> {
    let wim = find_path_case_insensitive(root, &["sources", "install.wim"])
        .filter(|node| !node.is_directory);
    let esd = find_path_case_insensitive(root, &["sources", "install.esd"])
        .filter(|node| !node.is_directory);
    let swm = find_path_case_insensitive(root, &["sources", "install.swm"])
        .filter(|node| !node.is_directory);
    if wim.is_none() && esd.is_none() && swm.is_none() {
        return Err("a ISO não contém sources/install.wim, install.esd ou install.swm".to_owned());
    }
    let oversized = wim
        .into_iter()
        .chain(esd)
        .find(|node| node.size > FAT32_MAX_FILE_BYTES);
    if oversized.is_some() {
        let has_swm_names = find_path_case_insensitive(root, &["sources"]).is_some_and(|sources| {
            sources.children.iter().any(|node| {
                let name = node.name.to_ascii_lowercase();
                name.strip_prefix("install")
                    .and_then(|name| name.strip_suffix(".swm"))
                    .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()))
            })
        });
        // Um único conjunto de SWMs não pode substituir duas imagens diferentes,
        // nem ser sobrescrito posteriormente por partes existentes na ISO.
        if (wim.is_some() && esd.is_some()) || has_swm_names {
            return Err("a ISO mistura imagens de instalação que conflitam com os arquivos install.swm gerados".to_owned());
        }
    }
    Ok(oversized)
}

fn prepare_split_wim(
    source: &mut File,
    wim: &TreeNode,
    iso_path: &Path,
    operation_id: &str,
    reporter: &mut Reporter,
    tracker: &mut ProgressTracker,
) -> Result<SplitArtifacts, String> {
    reporter.cancellation.check()?;
    let is_solid_esd = is_esd(wim);
    let required_bytes = required_wim_workspace_bytes(wim.size, is_solid_esd);
    let temporary_directory = create_wim_workspace(iso_path, required_bytes)?;
    let image_name = if is_solid_esd {
        "install.esd"
    } else {
        "install.wim"
    };
    let input_path = temporary_directory
        .path()
        .join(format!("source-{image_name}"));
    let output_path = temporary_directory.path().join("install.swm");

    send_status(
        reporter,
        operation_id,
        FlashPhase::Splitting,
        tracker.percentage(),
        split_progress_message(is_solid_esd),
    );
    let mut temporary_wim = File::create(&input_path)
        .map_err(|error| format!("não foi possível criar {image_name} temporário: {error}"))?;
    {
        let mut output = TrackedWriter::new(
            &mut temporary_wim,
            tracker,
            reporter,
            operation_id,
            FlashPhase::Splitting,
        );
        cat_node(source, wim, &mut output)
            .map_err(|error| format!("não foi possível extrair {image_name} da ISO: {error}"))?;
    }
    temporary_wim
        .sync_all()
        .map_err(|error| format!("não foi possível sincronizar o WIM temporário: {error}"))?;
    drop(temporary_wim);

    split_wim_with_progress(
        &input_path,
        &output_path,
        (estimated_split_bytes(wim.size, is_solid_esd), is_solid_esd),
        operation_id,
        reporter,
        tracker,
    )?;

    let parts = collect_and_validate_split_parts(temporary_directory.path())?;
    // A API já removeu o WIM intermediário. Libere também a origem extraída;
    // apenas os SWMs permanecem no workspace durante a gravação do pendrive.
    std::fs::remove_file(&input_path)
        .map_err(|error| format!("não foi possível liberar o WIM temporário: {error}"))?;

    Ok(SplitArtifacts {
        _temporary_directory: temporary_directory,
        source_path: format!("sources/{}", wim.name),
        parts,
    })
}

fn estimated_split_bytes(input_bytes: u64, is_solid_esd: bool) -> u64 {
    // Estimativa de expansão, não limite garantido de descompressão. Erros de
    // espaço na conversão são tratados antes de abrir/formatar o destino USB.
    input_bytes.saturating_mul(if is_solid_esd { 4 } else { 1 })
}

fn required_wim_workspace_bytes(input_bytes: u64, is_solid_esd: bool) -> u64 {
    let expanded = estimated_split_bytes(input_bytes, is_solid_esd);
    let payload = input_bytes
        .saturating_add(expanded)
        .saturating_add(if is_solid_esd { expanded } else { 0 });
    let reserve = (payload / 10).max(WIM_WORKSPACE_RESERVE_BYTES);
    payload.saturating_add(reserve)
}

fn split_progress_message(is_solid_esd: bool) -> &'static str {
    if is_solid_esd {
        "Extraindo e convertendo install.esd em partes SWM…"
    } else {
        "Dividindo install.wim em partes SWM…"
    }
}

fn create_wim_workspace(iso_path: &Path, required_bytes: u64) -> Result<TempDir, String> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "linux")]
    candidates.push(PathBuf::from("/var/tmp"));
    candidates.push(std::env::temp_dir());
    if let Some(parent) = iso_path.parent() {
        candidates.push(parent.to_path_buf());
    }

    create_wim_workspace_from_candidates(
        candidates,
        required_bytes,
        filesystem_available_space,
        |base| Builder::new().prefix("meraki-flash-wim-").tempdir_in(base),
    )
}

fn create_wim_workspace_from_candidates<A, C>(
    candidates: Vec<PathBuf>,
    required_bytes: u64,
    mut available_space: A,
    mut create_directory: C,
) -> Result<TempDir, String>
where
    A: FnMut(&Path) -> Result<u64, String>,
    C: FnMut(&Path) -> std::io::Result<TempDir>,
{
    let mut seen = HashSet::new();
    let mut attempts = Vec::new();
    for base in candidates {
        if !seen.insert(base.clone()) {
            continue;
        }
        let workspace = match create_directory(&base) {
            Ok(workspace) => workspace,
            Err(error) => {
                attempts.push(format!("{}: {error}", base.display()));
                continue;
            }
        };
        match available_space(workspace.path()) {
            Ok(available) if available >= required_bytes => return Ok(workspace),
            Ok(available) => attempts.push(format!(
                "{}: somente {} livres",
                base.display(),
                format_bytes(available)
            )),
            Err(error) => attempts.push(format!("{}: {error}", base.display())),
        }
    }

    Err(format!(
        "espaço temporário insuficiente para preparar a imagem Windows: são necessários aproximadamente {}. Locais verificados: {}. Libere espaço no disco do sistema ou mova a ISO para um volume com mais espaço livre",
        format_bytes(required_bytes),
        attempts.join("; ")
    ))
}

#[cfg(target_os = "linux")]
fn filesystem_available_space(path: &Path) -> Result<u64, String> {
    let statistics = nix::sys::statvfs::statvfs(path)
        .map_err(|error| format!("não foi possível consultar o espaço livre: {error}"))?;
    let fragment_size = (statistics.fragment_size() as u64).max(1);
    (statistics.blocks_available() as u64)
        .checked_mul(fragment_size)
        .ok_or_else(|| "o espaço livre retornado pelo sistema é inválido".to_owned())
}

#[cfg(target_os = "windows")]
fn filesystem_available_space(path: &Path) -> Result<u64, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut available = 0u64;
    // SAFETY: o caminho é UTF-16 terminado em NUL e o ponteiro de saída é válido.
    let success = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        return Err(format!(
            "não foi possível consultar o espaço livre: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(available)
}

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = (1024 * 1024 * 1024) as f64;
    const MIB: f64 = (1024 * 1024) as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB)
    }
}

fn collect_and_validate_split_parts(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut parts = std::fs::read_dir(directory)
        .map_err(|error| format!("não foi possível listar os SWMs temporários: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("swm"))
        })
        .collect::<Vec<_>>();
    parts.sort_by_key(|path| swm_part_number(path));
    if parts.is_empty() {
        return Err("wimlib não produziu nenhum arquivo SWM".to_owned());
    }

    for (index, part) in parts.iter().enumerate() {
        let expected_name = if index == 0 {
            "install.swm".to_owned()
        } else {
            format!("install{}.swm", index + 1)
        };
        let actual_name = part
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "wimlib produziu um nome SWM inválido".to_owned())?;
        if !actual_name.eq_ignore_ascii_case(&expected_name) {
            return Err(format!(
                "sequência SWM inválida: esperado {expected_name}, encontrado {actual_name}"
            ));
        }

        let size = part
            .metadata()
            .map_err(|error| format!("não foi possível consultar {}: {error}", part.display()))?
            .len();
        if size == 0 {
            return Err(format!("{actual_name} foi criado vazio"));
        }
        if size > SWM_PART_BYTES {
            return Err(format!(
                "{actual_name} excede o limite de {SWM_PART_BYTES} bytes por parte; a imagem contém um recurso indivisível muito grande"
            ));
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(part)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("não foi possível sincronizar {actual_name}: {error}"))?;
    }
    if parts.len() < 2 {
        return Err("wimlib não produziu a segunda parte install2.swm".to_owned());
    }
    Ok(parts)
}

fn swm_part_number(path: &Path) -> u32 {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    stem.strip_prefix("install")
        .filter(|suffix| !suffix.is_empty())
        .and_then(|suffix| suffix.parse().ok())
        .unwrap_or(1)
}

fn split_wim_with_progress(
    input_path: &Path,
    output_path: &Path,
    split_work: (u64, bool),
    operation_id: &str,
    reporter: &mut Reporter,
    tracker: &mut ProgressTracker,
) -> Result<(), String> {
    let (work_bytes, is_solid_esd) = split_work;
    let output_directory = output_path
        .parent()
        .ok_or_else(|| "diretório temporário SWM inválido".to_owned())?
        .to_path_buf();
    let input_path = input_path.to_path_buf();
    let output_path = output_path.to_path_buf();
    let cancellation = reporter.cancellation.clone();
    let worker = thread::spawn(move || {
        super::wim::split_or_export_image_cancellable(
            &input_path,
            &output_path,
            is_solid_esd,
            SWM_PART_BYTES,
            &cancellation,
        )
    });
    let mut mapped_progress = 0u64;

    while !worker.is_finished() {
        let produced = split_output_bytes(&output_directory).unwrap_or(mapped_progress);
        let mapped = produced.min(work_bytes);
        if mapped > mapped_progress {
            tracker.add(
                mapped - mapped_progress,
                FlashPhase::Splitting,
                reporter,
                operation_id,
                Some(split_progress_message(is_solid_esd).to_owned()),
            );
            mapped_progress = mapped;
        }
        thread::sleep(Duration::from_millis(250));
    }

    let result = worker
        .join()
        .map_err(|_| "o processo interno de divisão do WIM falhou".to_owned())?;
    reporter.cancellation.check()?;
    if let Err(error) = result {
        return Err(describe_split_failure(
            &output_directory,
            work_bytes,
            &error,
        ));
    }
    if mapped_progress < work_bytes {
        tracker.add(
            work_bytes - mapped_progress,
            FlashPhase::Splitting,
            reporter,
            operation_id,
            Some("Divisão do WIM concluída".to_owned()),
        );
    }
    Ok(())
}

fn describe_split_failure(directory: &Path, input_bytes: u64, error: &str) -> String {
    let produced = split_output_bytes(directory).unwrap_or(0);
    match filesystem_available_space(directory) {
        Ok(available)
            if available
                < input_bytes
                    .saturating_sub(produced)
                    .saturating_add(MIN_FREE_MARGIN_BYTES) =>
        {
            format!(
                "espaço temporário esgotado durante a divisão do WIM: {} livres em {}, {} já gerados. Detalhe: {error}",
                format_bytes(available),
                directory.display(),
                format_bytes(produced)
            )
        }
        Ok(available) => format!(
            "{error}. Área temporária: {} ({} livres; {} gerados)",
            directory.display(),
            format_bytes(available),
            format_bytes(produced)
        ),
        Err(space_error) => format!(
            "{error}. Área temporária: {} ({space_error})",
            directory.display()
        ),
    }
}

fn split_output_bytes(directory: &Path) -> Result<u64, String> {
    std::fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("swm"))
        })
        .try_fold(0u64, |total, path| {
            let size = path.metadata().map_err(|error| error.to_string())?.len();
            total
                .checked_add(size)
                .ok_or_else(|| "tamanho temporário SWM inválido".to_owned())
        })
}

#[allow(clippy::too_many_arguments)]
fn copy_directory<T: ReadWriteSeek>(
    source: &mut File,
    source_directory: &TreeNode,
    target_directory: &fatfs::Dir<'_, T>,
    parent_path: &str,
    split: Option<&SplitArtifacts>,
    operation_id: &str,
    reporter: &mut Reporter,
    tracker: &mut ProgressTracker,
) -> Result<(), String> {
    for node in &source_directory.children {
        reporter.cancellation.check()?;
        let relative_path = if parent_path.is_empty() {
            node.name.clone()
        } else {
            format!("{parent_path}/{}", node.name)
        };
        if node.is_directory {
            let directory = target_directory.create_dir(&node.name).map_err(|error| {
                format!("não foi possível criar {relative_path} no FAT32: {error}")
            })?;
            copy_directory(
                source,
                node,
                &directory,
                &relative_path,
                split,
                operation_id,
                reporter,
                tracker,
            )?;
            continue;
        }

        if (relative_path.eq_ignore_ascii_case("sources/install.wim")
            || relative_path.eq_ignore_ascii_case("sources/install.esd"))
            && let Some(split) = split
            && relative_path.eq_ignore_ascii_case(&split.source_path)
        {
            for part in &split.parts {
                reporter.cancellation.check()?;
                let name = part
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "nome temporário SWM inválido".to_owned())?;
                let mut input = BufReader::with_capacity(
                    COPY_BUFFER_SIZE,
                    File::open(part).map_err(|error| {
                        format!("não foi possível abrir {}: {error}", part.display())
                    })?,
                );
                let mut output = target_directory
                    .create_file(name)
                    .map_err(|error| format!("não foi possível criar sources/{name}: {error}"))?;
                output.truncate().map_err(|error| {
                    format!("não foi possível preparar sources/{name}: {error}")
                })?;
                let expected_bytes = part
                    .metadata()
                    .map_err(|error| format!("não foi possível consultar {name}: {error}"))?
                    .len();
                let copied_bytes = {
                    let mut tracked = TrackedWriter::new(
                        &mut output,
                        tracker,
                        reporter,
                        operation_id,
                        FlashPhase::Extracting,
                    );
                    copy_reader(&mut input, &mut tracked).map_err(|error| {
                        format!("não foi possível copiar sources/{name}: {error}")
                    })?
                };
                if copied_bytes != expected_bytes {
                    return Err(format!(
                        "sources/{name} ficou incompleto: esperado {expected_bytes} bytes, gravado {copied_bytes} bytes"
                    ));
                }
                output.flush().map_err(|error| {
                    format!("não foi possível finalizar sources/{name}: {error}")
                })?;
            }
            continue;
        }

        let mut output = target_directory
            .create_file(&node.name)
            .map_err(|error| format!("não foi possível criar {relative_path} no FAT32: {error}"))?;
        output
            .truncate()
            .map_err(|error| format!("não foi possível preparar {relative_path}: {error}"))?;
        if node.size > 0 {
            let mut tracked = TrackedWriter::new(
                &mut output,
                tracker,
                reporter,
                operation_id,
                FlashPhase::Extracting,
            );
            cat_node(source, node, &mut tracked)
                .map_err(|error| format!("não foi possível extrair {relative_path}: {error}"))?;
        }
    }
    Ok(())
}

fn copy_reader(reader: &mut impl Read, writer: &mut impl Write) -> std::io::Result<u64> {
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            writer.flush()?;
            return Ok(total);
        }
        writer.write_all(&buffer[..read])?;
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("tamanho copiado inválido"))?;
    }
}

fn write_unattend_file<T: ReadWriteSeek>(
    root_directory: &fatfs::Dir<'_, T>,
    content: &str,
) -> Result<(), String> {
    let mut file = root_directory
        .create_file("autounattend.xml")
        .map_err(|error| format!("não foi possível criar autounattend.xml: {error}"))?;
    file.truncate()
        .map_err(|error| format!("não foi possível substituir autounattend.xml: {error}"))?;
    file.write_all(content.as_bytes())
        .and_then(|_| file.flush())
        .map_err(|error| format!("não foi possível gravar autounattend.xml: {error}"))
}

struct ProgressTracker {
    total_bytes: u64,
    completed_bytes: u64,
    started: Instant,
    last_report: Instant,
}

impl ProgressTracker {
    fn new(total_bytes: u64) -> Self {
        Self {
            total_bytes,
            completed_bytes: 0,
            started: Instant::now(),
            last_report: Instant::now(),
        }
    }

    fn percentage(&self) -> f64 {
        self.completed_bytes as f64 * 100.0 / self.total_bytes as f64
    }

    fn add(
        &mut self,
        bytes: u64,
        phase: FlashPhase,
        reporter: &mut Reporter,
        operation_id: &str,
        message: Option<String>,
    ) {
        self.completed_bytes = self
            .completed_bytes
            .saturating_add(bytes)
            .min(self.total_bytes);
        if self.last_report.elapsed() < Duration::from_millis(250)
            && self.completed_bytes < self.total_bytes
            && message.is_none()
        {
            return;
        }
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let speed = self.completed_bytes as f64 / elapsed;
        let eta = (speed > 0.0)
            .then(|| ((self.total_bytes - self.completed_bytes) as f64 / speed).ceil() as u64);
        let _ = reporter.send(FlashProgress::new(
            operation_id,
            phase,
            self.percentage(),
            speed,
            eta,
            message,
        ));
        self.last_report = Instant::now();
    }
}

struct TrackedWriter<'a, W> {
    inner: W,
    tracker: &'a mut ProgressTracker,
    reporter: &'a mut Reporter,
    operation_id: &'a str,
    phase: FlashPhase,
}

impl<'a, W> TrackedWriter<'a, W> {
    fn new(
        inner: W,
        tracker: &'a mut ProgressTracker,
        reporter: &'a mut Reporter,
        operation_id: &'a str,
        phase: FlashPhase,
    ) -> Self {
        Self {
            inner,
            tracker,
            reporter,
            operation_id,
            phase,
        }
    }
}

impl<W: Write> Write for TrackedWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.reporter.cancellation.check_io()?;
        let written = self
            .inner
            .write(&buffer[..buffer.len().min(COPY_BUFFER_SIZE)])?;
        self.tracker.add(
            written as u64,
            self.phase,
            self.reporter,
            self.operation_id,
            None,
        );
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn send_status(
    reporter: &mut Reporter,
    operation_id: &str,
    phase: FlashPhase,
    percentage: f64,
    message: &str,
) {
    let _ = reporter.send(FlashProgress::new(
        operation_id,
        phase,
        percentage,
        0.0,
        None,
        Some(message.to_owned()),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom, Write};
    use tempfile::NamedTempFile;

    const TEST_DISK_BYTES: u64 = 3 * 1024 * 1024 * 1024;
    const TEST_SECTOR_BYTES: u32 = 512;

    #[test]
    fn cancelled_fat_copy_unmounts_and_removes_split_workspace() {
        use super::super::cancellation::{CancellationToken, is_cancelled_error};
        struct CancelOnSecondRead {
            file: File,
            token: CancellationToken,
            reads: usize,
        }
        impl Read for CancelOnSecondRead {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads == 2 {
                    self.token.cancel();
                }
                self.file.read(bytes)
            }
        }
        let workspace = tempfile::tempdir().unwrap();
        let workspace_path = workspace.path().to_path_buf();
        let part = workspace.path().join("install.swm");
        std::fs::write(&part, vec![0xaa; COPY_BUFFER_SIZE * 2]).unwrap();
        let split = SplitArtifacts {
            _temporary_directory: workspace,
            source_path: "sources/install.esd".to_owned(),
            parts: vec![part.clone()],
        };
        let mut disk = tempfile::tempfile().unwrap();
        disk.set_len(TEST_DISK_BYTES).unwrap();
        let (start, end) =
            create_partition_table(&mut disk, TEST_DISK_BYTES, TEST_SECTOR_BYTES).unwrap();
        format_fat32(&mut disk, start, end, TEST_SECTOR_BYTES).unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let mut reporter = Reporter {
            stream,
            token: "test".to_owned(),
            last_percentage: 0.0,
            cancellation: CancellationToken::default(),
            monitor: None,
        };
        let mut tracker = ProgressTracker::new((COPY_BUFFER_SIZE * 2) as u64);
        {
            let partition = StreamSlice::new(&mut disk, start, end).unwrap();
            let filesystem = FileSystem::new(BufStream::new(partition), FsOptions::new()).unwrap();
            {
                let root = filesystem.root_dir();
                let sources = root.create_dir("sources").unwrap();
                let file = sources.create_file("install.swm").unwrap();
                let mut reader = CancelOnSecondRead {
                    file: File::open(&part).unwrap(),
                    token: reporter.cancellation.clone(),
                    reads: 0,
                };
                let mut writer = TrackedWriter::new(
                    file,
                    &mut tracker,
                    &mut reporter,
                    "test",
                    FlashPhase::Extracting,
                );
                let error = copy_reader(&mut reader, &mut writer).unwrap_err();
                assert!(is_cancelled_error(&error.to_string()));
                writer.flush().unwrap();
            }
            let root = filesystem.root_dir();
            let error = copy_directory(
                &mut File::open(&part).unwrap(),
                &windows_tree_with_install_image("install.esd", FAT32_MAX_FILE_BYTES + 1),
                &root,
                "",
                Some(&split),
                "test",
                &mut reporter,
                &mut tracker,
            )
            .unwrap_err();
            assert!(is_cancelled_error(&error));
            drop(root);
            filesystem.unmount().unwrap();
        }
        disk.sync_all().unwrap();
        drop(split);
        assert!(!workspace_path.exists());
        let partition = StreamSlice::new(&mut disk, start, end).unwrap();
        let filesystem = FileSystem::new(BufStream::new(partition), FsOptions::new()).unwrap();
        {
            let root = filesystem.root_dir();
            let sources = root.open_dir("sources").unwrap();
            assert!(sources.open_file("install.esd").is_err());
            let mut bytes = Vec::new();
            sources
                .open_file("install.swm")
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes, vec![0xaa; COPY_BUFFER_SIZE]);
        }
        filesystem.unmount().unwrap();
    }
    const TEST_UNATTEND: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE"></settings>
</unattend>
"#;

    fn windows_tree_with_install_image(name: &str, size: u64) -> TreeNode {
        let mut root = TreeNode::new_directory("/".to_owned());
        let mut sources = TreeNode::new_directory("sources".to_owned());
        sources.add_child(TreeNode::new_file_with_location(
            name.to_owned(),
            size,
            0,
            size,
        ));
        root.add_child(sources);
        root.calculate_directory_size();
        root
    }

    #[test]
    fn permits_an_oversized_install_wim_for_later_splitting() {
        let size = FAT32_MAX_FILE_BYTES + 1;
        let tree = windows_tree_with_install_image("install.wim", size);

        let summary = validate_windows_tree(&tree, size).unwrap();

        assert_eq!(summary.total_file_bytes, size);
    }

    #[test]
    fn permits_an_oversized_install_esd_for_later_export_and_splitting() {
        let size = FAT32_MAX_FILE_BYTES + 1;
        let tree = windows_tree_with_install_image("INSTALL.ESD", size);
        assert_eq!(
            validate_windows_tree(&tree, size).unwrap().total_file_bytes,
            size
        );
        let selected = install_image_to_split(&tree).unwrap().unwrap();
        assert!(is_esd(selected));
        assert_eq!(selected.size, size);
    }

    #[test]
    fn leaves_install_images_within_the_fat32_limit_unchanged() {
        for name in ["install.wim", "install.esd"] {
            let tree = windows_tree_with_install_image(name, FAT32_MAX_FILE_BYTES);
            assert!(install_image_to_split(&tree).unwrap().is_none());
        }
    }

    #[test]
    fn rejects_conflicting_install_images_before_creating_swm_files() {
        for name in ["install.wim", "install.swm", "INSTALL2.SWM"] {
            let mut tree = windows_tree_with_install_image("install.esd", FAT32_MAX_FILE_BYTES + 1);
            tree.children[0].add_child(TreeNode::new_file_with_location(name.to_owned(), 1, 0, 1));
            assert!(
                install_image_to_split(&tree)
                    .unwrap_err()
                    .contains("conflitam")
            );
        }
    }

    #[test]
    fn rejects_other_files_that_exceed_the_fat32_limit() {
        let size = FAT32_MAX_FILE_BYTES + 1;
        let mut root = TreeNode::new_directory("/".to_owned());
        root.add_child(TreeNode::new_file_with_location(
            "payload.bin".to_owned(),
            size,
            0,
            size,
        ));

        let error = validate_windows_tree(&root, size).unwrap_err();

        assert!(error.contains("payload.bin"));
        assert!(error.contains("4 GiB"));
    }

    #[test]
    fn workspace_budget_covers_the_input_and_split_outputs() {
        let five_gib = 5 * 1024 * 1024 * 1024;

        let required = required_wim_workspace_bytes(five_gib, false);

        assert!(required >= five_gib * 2 + WIM_WORKSPACE_RESERVE_BYTES);
    }

    #[test]
    fn esd_workspace_covers_source_intermediate_parts_and_margin_without_overflow() {
        let size = 5 * 1024 * 1024 * 1024;
        let expanded = estimated_split_bytes(size, true);
        let required = required_wim_workspace_bytes(size, true);
        assert!(required >= size + 2 * expanded + WIM_WORKSPACE_RESERVE_BYTES);
        assert!(required > required_wim_workspace_bytes(size, false));
        assert_eq!(required_wim_workspace_bytes(u64::MAX, true), u64::MAX);
        assert_eq!(required_wim_workspace_bytes(u64::MAX, false), u64::MAX);
    }

    #[test]
    fn reports_the_correct_install_image_during_splitting() {
        assert_eq!(
            split_progress_message(true),
            "Extraindo e convertendo install.esd em partes SWM…"
        );
        assert_eq!(
            split_progress_message(false),
            "Dividindo install.wim em partes SWM…"
        );
    }

    #[test]
    fn workspace_skips_a_volume_without_enough_space() {
        let root = tempfile::tempdir().unwrap();
        let small = root.path().join("small");
        let large = root.path().join("large");
        std::fs::create_dir(&small).unwrap();
        std::fs::create_dir(&large).unwrap();
        let small_for_check = small.clone();
        let large_for_assertion = large.clone();

        let workspace = create_wim_workspace_from_candidates(
            vec![small, large],
            1_000,
            move |path| {
                if path.starts_with(&small_for_check) {
                    Ok(999)
                } else {
                    Ok(10_000)
                }
            },
            |base| Builder::new().prefix("workspace-").tempdir_in(base),
        )
        .unwrap();

        assert!(workspace.path().starts_with(large_for_assertion));
    }

    #[test]
    fn validates_a_complete_sequential_swm_set() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("install.swm"), b"part-one").unwrap();
        std::fs::write(directory.path().join("install2.swm"), b"part-two").unwrap();

        let parts = collect_and_validate_split_parts(directory.path()).unwrap();

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].file_name().unwrap(), "install.swm");
        assert_eq!(parts[1].file_name().unwrap(), "install2.swm");
    }

    #[test]
    fn copy_reader_reports_the_exact_number_of_bytes() {
        let payload = (0..(COPY_BUFFER_SIZE + 137))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut source = Cursor::new(payload.clone());
        let mut target = Vec::new();

        let copied = copy_reader(&mut source, &mut target).unwrap();

        assert_eq!(copied, payload.len() as u64);
        assert_eq!(target, payload);
    }

    #[test]
    fn validates_a_safe_unattend_document() {
        assert_eq!(
            validate_unattend_xml(Some(TEST_UNATTEND)).unwrap(),
            Some(TEST_UNATTEND.trim().to_owned())
        );
        assert_eq!(validate_unattend_xml(None).unwrap(), None);
    }

    #[test]
    fn rejects_malformed_or_unsafe_unattend_documents() {
        let malformed = "<unattend><settings></unattend>";
        let wrong_root = "<answer><settings /></answer>";
        let with_doctype = "<!DOCTYPE unattend><unattend><settings /></unattend>";
        let oversized = format!(
            "<unattend><settings>{}</settings></unattend>",
            " ".repeat(MAX_UNATTEND_BYTES)
        );

        assert!(validate_unattend_xml(Some(malformed)).is_err());
        assert!(validate_unattend_xml(Some(wrong_root)).is_err());
        assert!(validate_unattend_xml(Some(with_doctype)).is_err());
        assert!(validate_unattend_xml(Some(&oversized)).is_err());
    }

    #[test]
    fn creates_a_bootable_mbr_and_a_writable_fat32_filesystem() {
        assert_copies_split_image_to_fat32("install.wim");
    }

    #[test]
    fn replaces_install_esd_with_canonical_swm_files_on_fat32() {
        assert_copies_split_image_to_fat32("INSTALL.ESD");
    }

    fn assert_copies_split_image_to_fat32(image_name: &str) {
        let mut disk = NamedTempFile::new().unwrap();
        disk.as_file_mut().set_len(TEST_DISK_BYTES).unwrap();
        let split_directory = tempfile::tempdir().unwrap();
        let first_part = split_directory.path().join("install.swm");
        let second_part = split_directory.path().join("install2.swm");
        std::fs::write(&first_part, b"first-swm-part").unwrap();
        std::fs::write(&second_part, b"second-swm-part").unwrap();
        let split = SplitArtifacts {
            _temporary_directory: split_directory,
            source_path: format!("sources/{image_name}"),
            parts: vec![first_part, second_part],
        };
        // A ISO vazia faz cat_node falhar se a imagem original for copiada.
        let windows_tree = windows_tree_with_install_image(image_name, FAT32_MAX_FILE_BYTES + 1);
        let mut empty_iso = NamedTempFile::new().unwrap().reopen().unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let report_reader = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut messages = Vec::new();
            stream.read_to_end(&mut messages).unwrap();
            messages
        });
        let mut reporter = Reporter {
            stream: std::net::TcpStream::connect(address).unwrap(),
            token: "test-token".to_owned(),
            last_percentage: 0.0,
            cancellation: super::super::cancellation::CancellationToken::default(),
            monitor: None,
        };
        let mut tracker = ProgressTracker::new(64);

        let (start, end) =
            create_partition_table(disk.as_file_mut(), TEST_DISK_BYTES, TEST_SECTOR_BYTES).unwrap();
        disk.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
        let mbr = MBR::read_from(disk.as_file_mut(), TEST_SECTOR_BYTES).unwrap();
        assert_eq!(mbr[1].boot, BOOT_ACTIVE);
        assert_eq!(mbr[1].sys, 0x0c);
        assert_eq!(
            u64::from(mbr[1].starting_lba) * u64::from(TEST_SECTOR_BYTES),
            start
        );

        format_fat32(disk.as_file_mut(), start, end, TEST_SECTOR_BYTES).unwrap();
        {
            let partition = StreamSlice::new(disk.as_file_mut(), start, end).unwrap();
            let filesystem = FileSystem::new(BufStream::new(partition), FsOptions::new()).unwrap();
            let root = filesystem.root_dir();
            let efi = root.create_dir("EFI").unwrap();
            let mut marker = efi.create_file("MERAKI.TXT").unwrap();
            marker.write_all(b"fat32-ok").unwrap();
            write_unattend_file(&root, TEST_UNATTEND).unwrap();
            copy_directory(
                &mut empty_iso,
                &windows_tree,
                &root,
                "",
                Some(&split),
                "test-operation",
                &mut reporter,
                &mut tracker,
            )
            .unwrap();
            drop(marker);
            drop(efi);
            drop(root);
            filesystem.unmount().unwrap();
        }

        let partition = StreamSlice::new(disk.as_file_mut(), start, end).unwrap();
        let filesystem = FileSystem::new(BufStream::new(partition), FsOptions::new()).unwrap();
        let root = filesystem.root_dir();
        let efi = root.open_dir("EFI").unwrap();
        let mut marker = efi.open_file("MERAKI.TXT").unwrap();
        let mut contents = String::new();
        marker.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "fat32-ok");
        let mut unattend = root.open_file("autounattend.xml").unwrap();
        let mut xml = String::new();
        unattend.read_to_string(&mut xml).unwrap();
        assert_eq!(xml, TEST_UNATTEND);
        let sources = root.open_dir("sources").unwrap();
        let mut first = String::new();
        sources
            .open_file("install.swm")
            .unwrap()
            .read_to_string(&mut first)
            .unwrap();
        let mut second = String::new();
        sources
            .open_file("install2.swm")
            .unwrap()
            .read_to_string(&mut second)
            .unwrap();
        assert_eq!(first, "first-swm-part");
        assert_eq!(second, "second-swm-part");
        assert!(sources.open_file("install.wim").is_err());
        assert!(sources.open_file("install.esd").is_err());
        let names = sources
            .iter()
            .map(|entry| entry.unwrap().file_name().to_ascii_lowercase())
            .collect::<HashSet<_>>();
        assert!(names.contains("install.swm"));
        assert!(names.contains("install2.swm"));
        drop(reporter);
        let _messages = report_reader.join().unwrap();
    }
}
