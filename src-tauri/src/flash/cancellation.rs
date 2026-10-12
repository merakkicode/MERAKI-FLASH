use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const CANCELLED: &str = "MERAKI_OPERATION_CANCELLED";
pub const CANCELLED_MESSAGE: &str =
    "A gravação foi cancelada. O pendrive foi liberado, mas não está inicializável.";

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err(CANCELLED.to_owned())
        } else {
            Ok(())
        }
    }
    pub fn check_io(&self) -> io::Result<()> {
        self.check().map_err(io::Error::other)
    }
}

pub fn is_cancelled_error(error: &str) -> bool {
    error.contains(CANCELLED)
}

/// Interrompe novos dados; flush permanece permitido durante a finalização.
pub struct CancellableIo<T> {
    inner: T,
    token: CancellationToken,
}

impl<T> CancellableIo<T> {
    pub fn new(inner: T, token: &CancellationToken) -> Self {
        Self {
            inner,
            token: token.clone(),
        }
    }
}
impl<T: Read> Read for CancellableIo<T> {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        self.token.check_io()?;
        self.inner.read(data)
    }
}
impl<T: Write> Write for CancellableIo<T> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.token.check_io()?;
        self.inner
            .write(&data[..data.len().min(super::BUFFER_SIZE)])
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl<T: Seek> Seek for CancellableIo<T> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.token.check_io()?;
        self.inner.seek(from)
    }
}

/// Mantém o erro real de sincronização, inclusive quando a cópia foi cancelada.
pub fn finish_io(result: Result<(), String>, cleanup: io::Result<()>) -> Result<(), String> {
    cleanup.map_err(|error| {
        format!("falha ao sincronizar o dispositivo durante a finalização: {error}")
    })?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flash::{
        BUFFER_SIZE,
        image_stream::{copy_stream, verify_stream},
    };
    use std::io::Cursor;

    #[test]
    fn stops_raw_copy_after_one_block_and_preserves_remaining_sectors() {
        let token = CancellationToken::default();
        let mut source = Cursor::new(vec![0xaa; BUFFER_SIZE * 2]);
        let mut destination = tempfile::tempfile().unwrap();
        destination.write_all(&vec![0x55; BUFFER_SIZE * 2]).unwrap();
        destination.rewind().unwrap();
        let result = copy_stream(
            &mut CancellableIo::new(&mut source, &token),
            &mut CancellableIo::new(&mut destination, &token),
            (BUFFER_SIZE * 2) as u64,
            Some((BUFFER_SIZE * 2) as u64),
            |written| {
                assert_eq!(written, BUFFER_SIZE as u64);
                token.cancel();
            },
        );
        let error = finish_io(result.map(|_| ()), destination.sync_all()).unwrap_err();
        assert!(is_cancelled_error(&error));
        destination.rewind().unwrap();
        let mut bytes = Vec::new();
        destination.read_to_end(&mut bytes).unwrap();
        assert!(bytes[..BUFFER_SIZE].iter().all(|byte| *byte == 0xaa));
        assert!(bytes[BUFFER_SIZE..].iter().all(|byte| *byte == 0x55));
    }

    #[test]
    fn stops_verification_between_blocks() {
        let token = CancellationToken::default();
        let bytes = vec![0x55; BUFFER_SIZE * 2];
        let mut source = Cursor::new(&bytes);
        let mut target = Cursor::new(&bytes);
        let error = verify_stream(
            &mut CancellableIo::new(&mut source, &token),
            &mut CancellableIo::new(&mut target, &token),
            bytes.len() as u64,
            |_| token.cancel(),
        )
        .unwrap_err();
        assert!(is_cancelled_error(&error));
        assert_eq!(target.position(), BUFFER_SIZE as u64);
    }

    #[test]
    fn permits_flush_after_cancel_but_reports_cleanup_failures() {
        let token = CancellationToken::default();
        let mut io = CancellableIo::new(Cursor::new(Vec::new()), &token);
        token.cancel();
        assert!(io.write_all(b"must not write").is_err());
        io.flush().unwrap();
        let error = finish_io(token.check(), Err(io::Error::other("disk failure"))).unwrap_err();
        assert!(!is_cancelled_error(&error));
        assert!(error.contains("disk failure"));
    }
}
