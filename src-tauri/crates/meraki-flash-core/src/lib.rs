use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// Disco físico USB (removível ou fixo) que pode ser apresentado ao frontend.
///
/// `id` é adequado para seleção, mas o helper elevado ainda precisa revalidar
/// caminho, capacidade e serial imediatamente antes de abrir o bloco bruto.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsbDevice {
    pub id: String,
    pub name: String,
    pub device_path: String,
    pub mount_point: String,
    pub mount_points: Vec<String>,
    pub file_system: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub read_only: bool,
    pub kind: String,
    pub transport: String,
    pub serial: Option<String>,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("não foi possível executar {program}: {source}")]
    Command {
        program: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} retornou erro: {message}")]
    CommandFailed {
        program: &'static str,
        message: String,
    },
    #[error("resposta inválida de {program}: {source}")]
    InvalidOutput {
        program: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("a descoberta segura de USB não é suportada nesta plataforma")]
    UnsupportedPlatform,
}

pub fn discover_removable_devices() -> Result<Vec<UsbDevice>, DiscoveryError> {
    #[cfg(target_os = "linux")]
    {
        return linux::discover_removable_devices();
    }

    #[cfg(target_os = "windows")]
    {
        return windows::discover_removable_devices();
    }

    #[allow(unreachable_code)]
    Err(DiscoveryError::UnsupportedPlatform)
}

pub fn find_removable_device(id: &str) -> Result<Option<UsbDevice>, DiscoveryError> {
    Ok(discover_removable_devices()?
        .into_iter()
        .find(|device| device.id == id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Plain,
    Gzip,
    Xz,
    Zip,
}

impl ImageFormat {
    pub fn from_path(path: &Path) -> Result<Self, String> {
        match path.extension().and_then(|ext| ext.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
            "iso" | "img" | "raw" | "bin" => Ok(Self::Plain),
            "gz" => Ok(Self::Gzip),
            "xz" => Ok(Self::Xz),
            "zip" => Ok(Self::Zip),
            _ => Err("formato não suportado; selecione .iso, .img, .raw, .bin, .xz, .gz ou .zip".to_owned()),
        }
    }
}

/// Seleciona a primeira imagem regular na ordem do diretório central.
/// Não extrai caminhos do arquivo ZIP para o sistema de arquivos.
pub fn zip_image_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Result<(usize, u64), String> {
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).map_err(|error| format!("ZIP inválido: {error}"))?;
        if entry.is_file() && !entry.is_symlink()
            && ImageFormat::from_path(Path::new(entry.name())) == Ok(ImageFormat::Plain)
        {
            return Ok((index, entry.size()));
        }
    }
    Err("o ZIP não contém uma imagem .iso, .img, .raw ou .bin".to_owned())
}

/// Mantém a API anterior, agora aceitando todos os formatos de imagem suportados.
pub fn validate_iso_for_device(
    path: impl AsRef<Path>, device: &UsbDevice,
) -> Result<(PathBuf, u64), String> {
    validate_image_for_device(path, device)
}

/// Retorna o tamanho do arquivo de origem (compactado, quando aplicável).
/// GZ ISIZE é módulo 2^32 e não é um limite confiável para imagens grandes.
/// O gravador também deve limitar a quantidade real de bytes descompactados.
pub fn validate_image_for_device(
    iso_path: impl AsRef<Path>,
    device: &UsbDevice,
) -> Result<(PathBuf, u64), String> {
    let iso_path = iso_path
        .as_ref()
        .canonicalize()
        .map_err(|error| format!("não foi possível acessar a imagem: {error}"))?;
    let metadata = iso_path
        .metadata()
        .map_err(|error| format!("não foi possível ler a imagem: {error}"))?;

    if !metadata.is_file() {
        return Err("a imagem selecionada não é um arquivo regular".to_owned());
    }
    let format = ImageFormat::from_path(&iso_path)?;
    if metadata.len() == 0 {
        return Err("a imagem está vazia".to_owned());
    }
    if metadata.len() > device.total_bytes {
        return Err(format!(
            "o arquivo possui {} bytes, mas o dispositivo comporta apenas {} bytes",
            metadata.len(),
            device.total_bytes
        ));
    }

    if format == ImageFormat::Zip {
        let file = std::fs::File::open(&iso_path).map_err(|error| error.to_string())?;
        let mut archive = zip::ZipArchive::new(file).map_err(|error| format!("ZIP inválido: {error}"))?;
        let (_, size) = zip_image_entry(&mut archive)?;
        if size == 0 || size > device.total_bytes {
            return Err("a imagem descompactada do ZIP está vazia ou excede a capacidade do dispositivo".to_owned());
        }
    }

    if device.mount_points.iter().any(|mount_point| {
        let mount = Path::new(mount_point);
        mount.is_absolute() && mount != Path::new("/")
            && mount.canonicalize().is_ok_and(|mount| iso_path.starts_with(mount))
    }) {
        return Err("a imagem está armazenada no próprio dispositivo de destino".to_owned());
    }

    Ok((iso_path, metadata.len()))
}

pub(crate) fn stable_device_id(path: &str, serial: Option<&str>, size: u64) -> String {
    format!("{path}::{}::{size}", serial.unwrap_or("sem-serial"))
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    fn target() -> UsbDevice {
        UsbDevice {
            id: String::new(), name: String::new(), device_path: String::new(),
            mount_point: String::new(), mount_points: vec![], file_system: String::new(),
            total_bytes: 4096, available_bytes: 0, read_only: false,
            kind: "Fixed USB".to_owned(), transport: "USB".to_owned(), serial: None,
        }
    }

    #[test]
    fn validates_supported_extensions_and_rejects_unknown_empty_and_oversized_files() {
        let root = tempfile::tempdir().unwrap();
        let device = target();
        for name in ["disk.iso", "disk.IMG", "disk.raw", "disk.bin", "disk.img.xz", "disk.raw.gz"] {
            let path = root.path().join(name);
            std::fs::write(&path, [0; 512]).unwrap();
            assert_eq!(validate_image_for_device(&path, &device).unwrap().1, 512);
        }
        for name in ["disk.txt", "disk.exe", "disk.img.exe"] {
            let path = root.path().join(name);
            std::fs::write(&path, [0; 512]).unwrap();
            assert!(validate_image_for_device(path, &device).is_err());
        }
        let path = root.path().join("disk.raw");
        for size in [0, 4097] {
            std::fs::write(&path, vec![0; size]).unwrap();
            assert!(validate_image_for_device(&path, &device).is_err());
        }
        assert!(validate_image_for_device(root.path(), &device).is_err());
    }

    #[test]
    fn checks_zip_expanded_size_before_writing() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("disk.zip");
        let mut archive = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        archive.start_file("disk.img", zip::write::SimpleFileOptions::default()).unwrap();
        archive.write_all(&[0; 4097]).unwrap();
        archive.finish().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 4096);
        let mut device = target();
        assert!(validate_image_for_device(&path, &device).unwrap_err().contains("descompactada"));
        device.total_bytes = 8192;
        assert!(validate_image_for_device(path, &device).is_ok());
    }

    #[test]
    fn rejects_iso_on_destination_with_canonical_paths() {
        let root = std::env::temp_dir().join(format!("meraki-validation-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let iso = root.join("sample.ISO");
        std::fs::write(&iso, [0u8; 512]).unwrap();
        let mut device = UsbDevice {
            id: String::new(), name: String::new(), device_path: String::new(),
            mount_point: String::new(), mount_points: vec![root.to_string_lossy().into_owned()],
            file_system: String::new(), total_bytes: 1024, available_bytes: 0,
            read_only: false, kind: "Fixed USB".to_owned(), transport: "USB".to_owned(), serial: None,
        };
        assert!(validate_iso_for_device(&iso, &device).unwrap_err().contains("próprio dispositivo"));
        device.mount_points.clear();
        assert!(validate_iso_for_device(&iso, &device).is_ok());
        device.total_bytes = 256;
        assert!(validate_iso_for_device(&iso, &device).is_err());
        std::fs::remove_file(iso).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
