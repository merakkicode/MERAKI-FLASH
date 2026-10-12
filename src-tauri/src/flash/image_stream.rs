use flate2::read::MultiGzDecoder;
use lzma_rust2::XzReader;
use meraki_flash_core::{ImageFormat, zip_image_entry};
use std::cell::Cell;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::rc::Rc;

pub(super) struct ImageSource {
    file: File,
    format: ImageFormat,
    pub expanded_size: Option<u64>,
    source_size: u64,
}

/// O ZIP empresta seu leitor ao arquivo aberto apenas durante with_reader.
/// Isso evita estruturas autorreferenciais, threads e arquivos temporários.
enum ImageStream<'a> {
    Plain(BufReader<TrackedFile<'a>>),
    Gz(Box<MultiGzDecoder<BufReader<TrackedFile<'a>>>>),
    Xz(Box<XzReader<BufReader<TrackedFile<'a>>>>),
    Zip(zip::read::ZipFile<'a>),
}

impl Read for ImageStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(reader) => reader.read(buffer),
            Self::Gz(reader) => reader.read(buffer),
            Self::Xz(reader) => reader.read(buffer),
            Self::Zip(reader) => reader.read(buffer),
        }
    }
}

pub(super) struct StreamProgress {
    position: Rc<Cell<u64>>,
    start: u64,
    total: u64,
}

impl StreamProgress {
    pub fn fraction(&self) -> f64 {
        (self.position.get().saturating_sub(self.start) as f64 / self.total.max(1) as f64)
            .clamp(0.0, 1.0)
    }
}

struct TrackedFile<'a> {
    file: &'a mut File,
    position: Rc<Cell<u64>>,
}

impl Read for TrackedFile<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(output)?;
        self.position.set(self.position.get() + count as u64);
        Ok(count)
    }
}

impl Seek for TrackedFile<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = self.file.seek(from)?;
        self.position.set(position);
        Ok(position)
    }
}

impl ImageSource {
    pub fn open(path: &Path, capacity: u64) -> Result<Self, String> {
        let format = ImageFormat::from_path(path)?;
        let mut file = File::open(path).map_err(|error| format!("não foi possível abrir a imagem: {error}"))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        let source_size = metadata.len();
        if !metadata.is_file() || source_size == 0 || source_size > capacity {
            return Err("a imagem está vazia, não é um arquivo regular ou excede o dispositivo".to_owned());
        }
        let expanded_size = match format {
            ImageFormat::Plain => Some(source_size),
            ImageFormat::Zip => {
                let mut archive = zip::ZipArchive::new(&mut file).map_err(|error| format!("ZIP inválido: {error}"))?;
                let (index, size) = zip_image_entry(&mut archive)?;
                // Também rejeita criptografia e métodos não suportados antes de desmontar.
                let _ = archive.by_index(index).map_err(|error| format!("imagem ZIP não suportada: {error}"))?;
                Some(size)
            }
            ImageFormat::Gzip | ImageFormat::Xz => None,
        };
        if expanded_size.is_some_and(|size| size == 0 || size > capacity) {
            return Err("a imagem descompactada está vazia ou excede a capacidade do dispositivo".to_owned());
        }
        Ok(Self { file, format, expanded_size, source_size })
    }

    pub fn validate_header(&mut self) -> Result<(), String> {
        self.with_reader(|reader, _| {
            let mut header = [0; 512];
            reader.read_exact(&mut header).map_err(|error| format!("não foi possível ler o setor de boot: {error}"))?;
            if header[510..512] != [0x55, 0xaa] {
                return Err("a imagem não contém assinatura de boot MBR/GPT ou setor de boot tradicional (55 AA); confira o arquivo e o sistema selecionado".to_owned());
            }
            Ok(())
        })
    }

    pub fn with_reader<T>(
        &mut self,
        operation: impl FnOnce(&mut dyn Read, &StreamProgress) -> Result<T, String>,
    ) -> Result<T, String> {
        if self.file.metadata().map_err(|error| error.to_string())?.len() != self.source_size {
            return Err("o tamanho da imagem mudou durante a operação".to_owned());
        }
        self.file.seek(SeekFrom::Start(0)).map_err(|error| error.to_string())?;
        let position = Rc::new(Cell::new(0));
        let tracked = TrackedFile { file: &mut self.file, position: position.clone() };
        let mut progress = StreamProgress { position, start: 0, total: self.source_size };
        if self.format == ImageFormat::Zip {
            let mut archive = zip::ZipArchive::new(tracked).map_err(|error| format!("ZIP inválido: {error}"))?;
            let (index, size) = zip_image_entry(&mut archive)?;
            if Some(size) != self.expanded_size {
                return Err("o tamanho da imagem no ZIP mudou durante a operação".to_owned());
            }
            let entry = archive.by_index(index).map_err(|error| format!("falha ao abrir a imagem no ZIP: {error}"))?;
            progress.start = entry.data_start();
            progress.total = entry.compressed_size();
            return operation(&mut ImageStream::Zip(entry), &progress);
        }
        let reader = BufReader::with_capacity(64 * 1024, tracked);
        let mut stream = match self.format {
            ImageFormat::Plain => ImageStream::Plain(reader),
            ImageFormat::Gzip => ImageStream::Gz(Box::new(MultiGzDecoder::new(reader))),
            // 256 MiB de limite do decoder, inclusive para dicionários informados pelo arquivo.
            ImageFormat::Xz => ImageStream::Xz(Box::new(XzReader::new_mem_limit(reader, true, 256 * 1024))),
            ImageFormat::Zip => unreachable!(),
        };
        operation(&mut stream, &progress)
    }
}

fn read_chunk(reader: &mut dyn Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut count = 0;
    while count < buffer.len() {
        match reader.read(&mut buffer[count..]) {
            Ok(0) => break,
            Ok(read) => count += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(count)
}

pub(super) fn copy_stream(
    source: &mut dyn Read,
    target: &mut impl Write,
    capacity: u64,
    expected: Option<u64>,
    mut progress: impl FnMut(u64),
) -> Result<u64, String> {
    let mut buffer = vec![0; super::BUFFER_SIZE];
    let mut written = 0u64;
    loop {
        let count = read_chunk(source, &mut buffer).map_err(|error| format!("falha ao ler/descompactar a imagem: {error}"))?;
        if count == 0 { break; }
        let next = written.checked_add(count as u64).ok_or("tamanho da imagem inválido")?;
        if next > capacity || expected.is_some_and(|size| next > size) {
            return Err("a imagem descompactada excede a capacidade do dispositivo ou o tamanho declarado".to_owned());
        }
        target.write_all(&buffer[..count]).map_err(|error| format!("falha ao escrever no dispositivo: {error}"))?;
        written = next;
        progress(written);
    }
    if written == 0 || expected.is_some_and(|size| size != written) {
        return Err("a imagem está vazia ou seu tamanho descompactado difere do declarado".to_owned());
    }
    Ok(written)
}

pub(super) fn verify_stream(
    source: &mut dyn Read,
    target: &mut impl Read,
    written: u64,
    mut progress: impl FnMut(u64),
) -> Result<(), String> {
    let mut expected = vec![0; super::BUFFER_SIZE];
    let mut actual = vec![0; super::BUFFER_SIZE];
    let mut verified = 0u64;
    loop {
        // Lê até EOF para validar também CRC/rodapés e membros concatenados.
        let count = read_chunk(source, &mut expected).map_err(|error| format!("falha ao reler/descompactar a imagem: {error}"))?;
        if count == 0 { break; }
        if count as u64 > written.saturating_sub(verified) {
            return Err("a imagem mudou de tamanho durante a verificação".to_owned());
        }
        target.read_exact(&mut actual[..count]).map_err(|error| format!("falha ao verificar o dispositivo: {error}"))?;
        if expected[..count] != actual[..count] {
            return Err(format!("a verificação encontrou dados diferentes a partir do byte {verified}; grave novamente antes de usar o dispositivo"));
        }
        verified += count as u64;
        progress(verified);
    }
    if verified != written { return Err("a imagem mudou de tamanho durante a verificação".to_owned()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flash::sector_io::SectorIo;
    use flate2::{Compression, write::GzEncoder};
    use lzma_rust2::{XzOptions, XzWriter};
    use std::io::Cursor;
    use tempfile::{Builder, NamedTempFile};
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    fn payload() -> Vec<u8> {
        let mut bytes = (0..super::super::BUFFER_SIZE + 137).map(|n| (n % 251) as u8).collect::<Vec<_>>();
        bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
        bytes
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = XzWriter::new(Vec::new(), XzOptions::with_preset(1)).unwrap();
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn zip(bytes: &[u8], method: CompressionMethod) -> Vec<u8> {
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(method);
        archive.start_file("README.txt", options).unwrap();
        archive.write_all(b"not an image").unwrap();
        archive.start_file("nested/disk.IMG", options).unwrap();
        archive.write_all(bytes).unwrap();
        archive.start_file("second.raw", options).unwrap();
        archive.write_all(b"must not be copied").unwrap();
        archive.finish().unwrap().into_inner()
    }

    fn source(suffix: &str, bytes: &[u8]) -> NamedTempFile {
        let mut file = Builder::new().suffix(suffix).tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn streams_raw_gz_xz_and_zip_and_restarts_for_verification() {
        let payload = payload();
        for (suffix, encoded) in [
            (".img", payload.clone()), (".img.gz", gzip(&payload)),
            (".raw.xz", xz(&payload)), (".zip", zip(&payload, CompressionMethod::Deflated)),
            (".ZIP", zip(&payload, CompressionMethod::Stored)),
        ] {
            let file = source(suffix, &encoded);
            for sector in [512, 4096] {
                let capacity = (payload.len() as u64).div_ceil(sector as u64) * sector as u64 + 4096;
                let mut image = ImageSource::open(file.path(), capacity).unwrap();
                image.validate_header().unwrap();
                let expected = image.expanded_size;
                let mut target = SectorIo::new(Cursor::new(vec![0xcc; capacity as usize]), sector, capacity).unwrap();
                let mut last_fraction = 0.0;
                let written = image.with_reader(|reader, progress| {
                    copy_stream(reader, &mut target, capacity, expected, |_| {
                        let fraction = progress.fraction();
                        assert!(fraction >= last_fraction && fraction <= 1.0);
                        last_fraction = fraction;
                    })
                }).unwrap();
                assert_eq!(written, payload.len() as u64, "{suffix}");
                target.seek(SeekFrom::Start(0)).unwrap();
                image.with_reader(|reader, _| verify_stream(reader, &mut target, written, |_| {})).unwrap();
                let disk = target.finish().unwrap().into_inner();
                assert_eq!(&disk[..payload.len()], payload.as_slice(), "{suffix}");
                assert!(disk[payload.len()..].iter().all(|byte| *byte == 0xcc));
            }
        }
    }

    #[test]
    fn concatenated_gzip_and_xz_members_are_not_silently_truncated() {
        let payload = payload();
        for (suffix, compress) in [(".gz", gzip as fn(&[u8]) -> Vec<u8>), (".xz", xz)] {
            let encoded = [compress(&payload[..512]), compress(&payload[512..])].concat();
            let file = source(suffix, &encoded);
            let mut image = ImageSource::open(file.path(), payload.len() as u64).unwrap();
            let mut output = Vec::new();
            image.with_reader(|reader, _| copy_stream(reader, &mut output, payload.len() as u64, None, |_| {})).unwrap();
            assert_eq!(output, payload);
        }
    }

    #[test]
    fn rejects_corruption_and_truncation_in_compressed_streams() {
        let payload = payload();
        for (suffix, encoded) in [(".gz", gzip(&payload)), (".xz", xz(&payload))] {
            let mut corrupt = encoded.clone();
            let length = corrupt.len();
            corrupt[length - 8] ^= 0xff;
            for bytes in [corrupt, encoded[..encoded.len() - 5].to_vec()] {
                let file = source(suffix, &bytes);
                let mut image = ImageSource::open(file.path(), payload.len() as u64).unwrap();
                assert!(image.with_reader(|reader, _| copy_stream(reader, &mut io::sink(), payload.len() as u64, None, |_| {})).is_err(), "{suffix}");
            }
        }
        let mut encoded = zip(&payload, CompressionMethod::Stored);
        let mut archive = zip::ZipArchive::new(Cursor::new(&encoded)).unwrap();
        let start = archive.by_index(1).unwrap().data_start() as usize;
        drop(archive);
        encoded[start + 512] ^= 0xff;
        let file = source(".zip", &encoded);
        let mut image = ImageSource::open(file.path(), (encoded.len() as u64) + 4096).unwrap();
        assert!(image.with_reader(|reader, _| copy_stream(reader, &mut io::sink(), u64::MAX, None, |_| {})).is_err());
    }

    #[test]
    fn rejects_oversized_sources_and_bounds_unknown_expanded_size() {
        let payload = payload();
        let raw = source(".raw", &payload);
        assert!(ImageSource::open(raw.path(), 512).is_err());
        let archive = source(".zip", &zip(&payload, CompressionMethod::Deflated));
        assert!(ImageSource::open(archive.path(), payload.len() as u64 - 1).is_err());
        for (suffix, encoded) in [(".gz", gzip(&payload)), (".xz", xz(&payload))] {
            let file = source(suffix, &encoded);
            let capacity = super::super::BUFFER_SIZE as u64;
            let mut image = ImageSource::open(file.path(), capacity).unwrap();
            let mut target = Cursor::new(vec![0xcc; capacity as usize + 512]);
            assert!(image.with_reader(|reader, _| copy_stream(reader, &mut target, capacity, None, |_| {})).is_err());
            assert_eq!(target.position(), capacity);
            assert!(target.into_inner()[capacity as usize..].iter().all(|b| *b == 0xcc));
        }
    }

    #[test]
    fn accepts_mbr_gpt_and_traditional_boot_sector_but_rejects_missing_signature() {
        for partition_type in [0, 0x83, 0xee] {
            let mut bytes = vec![0; 512];
            bytes[450] = partition_type;
            bytes[510..].copy_from_slice(&[0x55, 0xaa]);
            let file = source(".img", &bytes);
            ImageSource::open(file.path(), 1024).unwrap().validate_header().unwrap();
        }
        for bytes in [vec![0; 512], vec![0; 511]] {
            let file = source(".gz", &gzip(&bytes));
            assert!(ImageSource::open(file.path(), 1024).unwrap().validate_header().is_err());
        }
    }

    #[test]
    fn verification_detects_changed_bytes_and_lengths() {
        assert!(verify_stream(&mut Cursor::new([1, 2]), &mut Cursor::new([1, 3]), 2, |_| {}).is_err());
        assert!(verify_stream(&mut Cursor::new([1]), &mut Cursor::new([1, 2]), 2, |_| {}).is_err());
        assert!(verify_stream(&mut Cursor::new([1, 2, 3]), &mut Cursor::new([1, 2]), 2, |_| {}).is_err());
    }

    #[test]
    fn rejects_zip_without_image_and_unknown_extensions() {
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        archive.start_file("notes.txt", SimpleFileOptions::default()).unwrap();
        archive.write_all(b"notes").unwrap();
        let file = source(".zip", &archive.finish().unwrap().into_inner());
        assert!(ImageSource::open(file.path(), 4096).is_err());
        for extension in [".txt", ".exe"] {
            let file = source(extension, &[0; 512]);
            assert!(ImageSource::open(file.path(), 4096).is_err());
        }
    }
}
