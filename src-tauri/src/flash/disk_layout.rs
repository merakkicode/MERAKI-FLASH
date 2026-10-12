use super::cancellation::{CancellableIo, CancellationToken, finish_io};
use fatfs::{FatType, FormatVolumeOptions};
use fscommon::{BufStream, StreamSlice};
use mbrman::{BOOT_ACTIVE, CHS, MBR, MBRPartitionEntry};
use std::io::{Read, Seek, SeekFrom, Write};
use uuid::Uuid;
pub(super) const ALIGNMENT_BYTES: u64 = 1024 * 1024;
const CLEAR_BYTES: u64 = 2 * 1024 * 1024;
const COPY_BUFFER_SIZE: usize = 1024 * 1024;
pub(super) fn partition_bounds(device_size: u64, sector_size: u32) -> Result<(u64, u64), String> {
    if !(512..=4096).contains(&sector_size)
        || !sector_size.is_power_of_two()
        || !device_size.is_multiple_of(u64::from(sector_size))
    {
        return Err("geometria de setores inválida".to_owned());
    }
    let sector_size_u64 = u64::from(sector_size);
    let start = align_up(ALIGNMENT_BYTES, sector_size_u64);
    let end = align_down(device_size.saturating_sub(ALIGNMENT_BYTES), sector_size_u64);
    if end <= start {
        return Err("o dispositivo é pequeno demais para FAT32".to_owned());
    }
    let disk_sectors = device_size / sector_size_u64;
    let partition_sectors = (end - start) / sector_size_u64;
    if device_size > 2 * 1024_u64.pow(4)
        || disk_sectors > u64::from(u32::MAX)
        || partition_sectors > u64::from(u32::MAX)
    {
        return Err("dispositivos maiores que 2 TiB ainda não são suportados".to_owned());
    }
    if partition_sectors < 70_000 {
        return Err(
            "o dispositivo é pequeno demais para FAT32 com este tamanho de setor".to_owned(),
        );
    }
    Ok((start, end))
}

pub(super) fn create_partition_table(
    target: &mut (impl Read + Write + Seek),
    device_size: u64,
    sector_size: u32,
) -> Result<(u64, u64), String> {
    // Todas as limitações são conferidas antes de apagar o primeiro setor.
    let (start, end) = partition_bounds(device_size, sector_size)?;
    let sector_size_u64 = u64::from(sector_size);
    let partition_sectors = (end - start) / sector_size_u64;

    clear_range(target, 0, CLEAR_BYTES.min(device_size))?;
    if device_size > CLEAR_BYTES {
        clear_range(target, device_size - CLEAR_BYTES, CLEAR_BYTES)?;
    }

    let signature = Uuid::new_v4();
    let signature: [u8; 4] = signature.as_bytes()[..4]
        .try_into()
        .map_err(|_| "não foi possível gerar a assinatura do disco".to_owned())?;
    let mut bounded = StreamSlice::new(&mut *target, 0, device_size)
        .map_err(|error| format!("não foi possível delimitar o dispositivo: {error}"))?;
    let mut mbr = MBR::new_from(&mut bounded, sector_size, signature)
        .map_err(|error| format!("não foi possível criar a tabela MBR: {error}"))?;
    mbr[1] = MBRPartitionEntry {
        boot: BOOT_ACTIVE,
        first_chs: CHS::empty(),
        sys: 0x0c,
        last_chs: CHS::empty(),
        starting_lba: u32::try_from(start / sector_size_u64)
            .map_err(|_| "início da partição fora do limite MBR".to_owned())?,
        sectors: u32::try_from(partition_sectors)
            .map_err(|_| "partição fora do limite MBR".to_owned())?,
    };
    mbr.write_into(&mut bounded)
        .map_err(|error| format!("não foi possível gravar a tabela MBR: {error}"))?;
    bounded
        .flush()
        .map_err(|error| format!("não foi possível finalizar a tabela MBR: {error}"))?;
    Ok((start, end))
}

#[cfg(test)]
pub(super) fn format_fat32(
    target: &mut (impl Read + Write + Seek),
    start: u64,
    end: u64,
    sector_size: u32,
) -> Result<(), String> {
    format_fat32_cancellable(
        target,
        start,
        end,
        sector_size,
        &CancellationToken::default(),
        *b"MERAKI     ",
    )
}

pub(super) fn format_fat32_cancellable(
    target: &mut (impl Read + Write + Seek),
    start: u64,
    end: u64,
    sector_size: u32,
    cancellation: &CancellationToken,
    label: [u8; 11],
) -> Result<(), String> {
    let partition_sectors = (end - start) / u64::from(sector_size);
    let options = FormatVolumeOptions::new()
        .bytes_per_sector(
            u16::try_from(sector_size)
                .map_err(|_| format!("tamanho de setor inválido: {sector_size}"))?,
        )
        .fat_type(FatType::Fat32)
        .total_sectors(
            u32::try_from(partition_sectors)
                .map_err(|_| "a partição FAT32 excede o limite suportado".to_owned())?,
        )
        .volume_id(Uuid::new_v4().as_u128() as u32)
        .volume_label(label);
    let partition = StreamSlice::new(&mut *target, start, end)
        .map_err(|error| format!("não foi possível delimitar a partição FAT32: {error}"))?;
    let mut buffered = BufStream::new(partition);
    let result = fatfs::format_volume(CancellableIo::new(&mut buffered, cancellation), options)
        .map_err(|error| format!("não foi possível formatar a partição FAT32: {error}"));
    finish_io(result, buffered.flush())
}

fn clear_range(
    target: &mut (impl Read + Write + Seek),
    start: u64,
    length: u64,
) -> Result<(), String> {
    target
        .seek(SeekFrom::Start(start))
        .map_err(|error| format!("não foi possível posicionar o dispositivo: {error}"))?;
    let zeros = vec![0u8; COPY_BUFFER_SIZE.min(length as usize)];
    let mut remaining = length;
    while remaining > 0 {
        let count = remaining.min(zeros.len() as u64) as usize;
        target
            .write_all(&zeros[..count])
            .map_err(|error| format!("não foi possível limpar assinaturas antigas: {error}"))?;
        remaining -= count as u64;
    }
    Ok(())
}

const fn align_up(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment) * alignment
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value / alignment * alignment
}
