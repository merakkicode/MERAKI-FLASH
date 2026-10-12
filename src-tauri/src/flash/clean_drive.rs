use super::cancellation::{CancellableIo, CancellationToken, finish_io};
use super::disk_layout::{create_partition_table, format_fat32_cancellable, partition_bounds};
use super::protocol::{FlashPhase, FlashProgress};
use super::sector_io::SectorIo;
use super::{Reporter, platform};
use meraki_flash_core::UsbDevice;
use std::io::{Read, Seek, Write};

pub(super) fn normalize_label(label: Option<&str>) -> Result<String, String> {
    let label = label.unwrap_or("PENDRIVE");
    if label.is_empty()
        || label.len() > 11
        || !label.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(
            "o rótulo deve ter de 1 a 11 letras ASCII ou números, sem espaços ou símbolos"
                .to_owned(),
        );
    }
    Ok(label.to_ascii_uppercase())
}

fn restore(
    file: &mut (impl Read + Write + Seek),
    size: u64,
    sector: u32,
    label: &str,
    cancellation: &CancellationToken,
    mut progress: impl FnMut(FlashPhase, f64, &str),
) -> Result<(), String> {
    let label = normalize_label(Some(label))?;
    let mut padded = [b' '; 11];
    padded[..label.len()].copy_from_slice(label.as_bytes());
    partition_bounds(size, sector)?;
    cancellation.check()?;
    let mut target = SectorIo::new(file, sector, size).map_err(|error| error.to_string())?;
    progress(
        FlashPhase::Formatting,
        10.0,
        "Limpando partições antigas e criando tabela MBR…",
    );
    let (start, end) = create_partition_table(
        &mut CancellableIo::new(&mut target, cancellation),
        size,
        sector,
    )?;
    progress(FlashPhase::Formatting, 40.0, "Formatando partição FAT32…");
    format_fat32_cancellable(&mut target, start, end, sector, cancellation, padded)?;
    cancellation.check()?;
    target
        .finish()
        .map_err(|error| format!("falha ao finalizar os setores: {error}"))?;
    Ok(())
}

pub(super) fn execute(
    device: &UsbDevice,
    label: Option<&str>,
    operation_id: &str,
    reporter: &mut Reporter,
) -> Result<(), String> {
    let label = normalize_label(label)?;
    reporter.cancellation.check()?;
    reporter.send(FlashProgress::new(
        operation_id,
        FlashPhase::Preparing,
        0.0,
        0.0,
        None,
        Some("Desmontando e bloqueando o pendrive…".to_owned()),
    ))?;
    let mut prepared = platform::prepare_device(device)?;
    let sector = platform::logical_sector_size(&prepared.file)?;
    let cancellation = reporter.cancellation.clone();
    let result = restore(
        &mut prepared.file,
        device.total_bytes,
        sector,
        &label,
        &cancellation,
        |phase, percentage, message| {
            let _ = reporter.send(FlashProgress::new(
                operation_id,
                phase,
                percentage,
                0.0,
                None,
                Some(message.to_owned()),
            ));
        },
    );
    let sync_percentage = if result.is_ok() {
        90.0
    } else {
        reporter.last_percentage()
    };
    let _ = reporter.send(FlashProgress::new(
        operation_id,
        FlashPhase::Syncing,
        sync_percentage,
        0.0,
        None,
        Some("Sincronizando o dispositivo…".to_owned()),
    ));
    let result = finish_io(result, prepared.file.sync_all());
    if result.is_ok() {
        prepared.release_and_refresh()?;
    } else {
        drop(prepared);
    }
    result?;
    cancellation.check()
}

#[cfg(test)]
mod tests {
    use super::super::cancellation::is_cancelled_error;
    use super::*;
    use fatfs::{FileSystem, FsOptions};
    use fscommon::{BufStream, StreamSlice};
    use mbrman::{BOOT_ACTIVE, MBR};
    use std::io::{Read, SeekFrom, Write};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    const SIZE: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    #[test]
    fn restores_sparse_one_gib_disk_and_clears_both_gpt_headers() {
        for sector in [512, 4096] {
            let mut file = tempfile::tempfile().unwrap();
            file.set_len(SIZE).unwrap();
            file.write_all(&vec![0xa5; (2 * MIB) as usize]).unwrap();
            file.seek(SeekFrom::Start(SIZE - 2 * MIB)).unwrap();
            file.write_all(&vec![0xa5; (2 * MIB) as usize]).unwrap();
            restore(
                &mut file,
                SIZE,
                sector,
                "meusdados",
                &CancellationToken::default(),
                |_, _, _| {},
            )
            .unwrap();
            file.sync_all().unwrap();
            file.rewind().unwrap();
            let mbr = MBR::read_from(&mut file, sector).unwrap();
            assert_eq!(mbr.iter().filter(|(_, entry)| entry.sys != 0).count(), 1);
            assert_eq!(mbr[1].boot, BOOT_ACTIVE);
            assert_eq!(mbr[1].sys, 0x0c);
            assert_eq!(u64::from(mbr[1].starting_lba) * u64::from(sector), MIB);
            assert_eq!(
                u64::from(mbr[1].sectors) * u64::from(sector),
                SIZE - 2 * MIB
            );
            file.seek(SeekFrom::Start(u64::from(sector))).unwrap();
            let mut bytes = vec![0; (MIB - u64::from(sector)) as usize];
            file.read_exact(&mut bytes).unwrap();
            assert!(bytes.iter().all(|byte| *byte == 0));
            file.seek(SeekFrom::Start(SIZE - 2 * MIB)).unwrap();
            let mut tail = vec![0; (2 * MIB) as usize];
            file.read_exact(&mut tail).unwrap();
            assert!(tail.iter().all(|byte| *byte == 0));
            let part = StreamSlice::new(&mut file, MIB, SIZE - MIB).unwrap();
            let fs = FileSystem::new(BufStream::new(part), FsOptions::new()).unwrap();
            assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
            assert_eq!(fs.volume_label().trim(), "MEUSDADOS");
            {
                let root = fs.root_dir();
                let mut marker = root.create_file("TEST.TXT").unwrap();
                marker.write_all(b"restored").unwrap();
                marker.rewind().unwrap();
                let mut contents = String::new();
                marker.read_to_string(&mut contents).unwrap();
                assert_eq!(contents, "restored");
            }
            fs.unmount().unwrap();
        }
    }

    struct CancelDuringFormat {
        file: std::fs::File,
        token: CancellationToken,
        armed: Arc<AtomicBool>,
    }
    impl Read for CancelDuringFormat {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.file.read(bytes)
        }
    }
    impl Seek for CancelDuringFormat {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            self.file.seek(from)
        }
    }
    impl Write for CancelDuringFormat {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let count = self.file.write(bytes)?;
            if self.armed.swap(false, Ordering::AcqRel) {
                self.token.cancel();
            }
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }

    #[test]
    fn interrupts_during_fat_formatting_and_allows_sync() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(SIZE).unwrap();
        let token = CancellationToken::default();
        let armed = Arc::new(AtomicBool::new(false));
        let mut target = CancelDuringFormat {
            file,
            token: token.clone(),
            armed: armed.clone(),
        };
        let result = restore(
            &mut target,
            SIZE,
            512,
            "PENDRIVE",
            &token,
            |_, percentage, _| {
                if percentage == 40.0 {
                    armed.store(true, Ordering::Release);
                }
            },
        );
        let error = finish_io(result, target.file.sync_all()).unwrap_err();
        assert!(is_cancelled_error(&error));
    }

    #[test]
    fn validates_labels_and_geometry_before_any_write() {
        assert_eq!(normalize_label(None).unwrap(), "PENDRIVE");
        assert_eq!(normalize_label(Some("usb123")).unwrap(), "USB123");
        for label in [
            "",
            "123456789012",
            "USB DISK",
            "ação",
            "A/B",
            "A.B",
            "A-B",
            "A\0B",
        ] {
            assert!(normalize_label(Some(label)).is_err());
        }
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"unchanged").unwrap();
        for (size, sector) in [
            (1024, 512),
            (SIZE, 0),
            (SIZE, 513),
            (SIZE + 1, 512),
            (3 * 1024_u64.pow(4), 512),
        ] {
            assert!(
                restore(
                    &mut file,
                    size,
                    sector,
                    "PENDRIVE",
                    &CancellationToken::default(),
                    |_, _, _| {}
                )
                .is_err()
            );
        }
        let cancelled = CancellationToken::default();
        cancelled.cancel();
        assert!(is_cancelled_error(
            &restore(&mut file, SIZE, 512, "PENDRIVE", &cancelled, |_, _, _| {}).unwrap_err()
        ));
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"unchanged");
    }
}
