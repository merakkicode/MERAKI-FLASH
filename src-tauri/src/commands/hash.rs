use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

const BUFFER_BYTES: usize = 2 * 1024 * 1024;
const CANCELLED: &str = "Cálculo de hash cancelado.";

#[derive(Clone, Default)]
pub struct HashManager(Arc<Mutex<HashState>>);

#[derive(Default)]
struct HashState {
    active: Option<(String, Arc<AtomicBool>)>,
    // Cancelamento pode chegar antes de compute_image_hash ser despachado.
    cancelled_requests: VecDeque<String>,
}

struct HashJob {
    id: String,
    cancelled: Arc<AtomicBool>,
    manager: HashManager,
}

impl HashManager {
    fn begin(&self, id: String) -> Result<HashJob, String> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "gerenciador de hash indisponível")?;
        if let Some(index) = state
            .cancelled_requests
            .iter()
            .position(|value| value == &id)
        {
            state.cancelled_requests.remove(index);
            return Err(CANCELLED.to_owned());
        }
        if state
            .active
            .as_ref()
            .is_some_and(|(active, _)| active == &id)
        {
            return Err("este cálculo já está em andamento".to_owned());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        if let Some((_, previous)) = state.active.replace((id.clone(), cancelled.clone())) {
            previous.store(true, Ordering::Release);
        }
        Ok(HashJob {
            id,
            cancelled,
            manager: self.clone(),
        })
    }

    fn cancel(&self, id: Option<&str>) -> Result<(), String> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "gerenciador de hash indisponível")?;
        if let Some((active, cancelled)) = &state.active
            && id.is_none_or(|id| id == active)
        {
            cancelled.store(true, Ordering::Release);
            return Ok(());
        }
        if let Some(id) = id
            && !state.cancelled_requests.iter().any(|value| value == id)
        {
            if state.cancelled_requests.len() == 64 {
                state.cancelled_requests.pop_front();
            }
            state.cancelled_requests.push_back(id.to_owned());
        }
        Ok(())
    }
}

impl Drop for HashJob {
    fn drop(&mut self) {
        if let Ok(mut state) = self.manager.0.lock()
            && state.active.as_ref().is_some_and(|(id, _)| id == &self.id)
        {
            state.active = None;
        }
    }
}

#[derive(Clone, Copy)]
enum Algorithm {
    Sha256,
    Md5,
}

impl Algorithm {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value
            .unwrap_or("sha256")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "sha256" => Ok(Self::Sha256),
            "md5" => Ok(Self::Md5),
            _ => Err("algoritmo inválido; use sha256 ou md5".to_owned()),
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HashProgress {
    request_id: String,
    percentage: f64,
    bytes_processed: u64,
    total_bytes: u64,
    bytes_per_second: f64,
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err(CANCELLED.to_owned())
    } else {
        Ok(())
    }
}

fn hash_file(
    path: &Path,
    algorithm: Algorithm,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<String, String> {
    check_cancelled(cancelled)?;
    let file =
        File::open(path).map_err(|error| format!("não foi possível abrir a imagem: {error}"))?;
    let before = file
        .metadata()
        .map_err(|error| format!("não foi possível consultar a imagem: {error}"))?;
    if !before.is_file() {
        return Err("selecione um arquivo regular de imagem".to_owned());
    }
    let total = before.len();
    let mut reader = BufReader::with_capacity(BUFFER_BYTES, file);
    let mut sha = Sha256::new();
    let mut md5 = md5::Context::new();
    let mut processed = 0u64;
    progress(0, total);
    loop {
        check_cancelled(cancelled)?;
        let bytes = reader
            .fill_buf()
            .map_err(|error| format!("falha ao ler a imagem: {error}"))?;
        check_cancelled(cancelled)?;
        if bytes.is_empty() {
            break;
        }
        if bytes.len() as u64 > total.saturating_sub(processed) {
            return Err(
                "a imagem mudou durante o cálculo; selecione-a e calcule novamente".to_owned(),
            );
        }
        match algorithm {
            Algorithm::Sha256 => sha.update(bytes),
            Algorithm::Md5 => md5.consume(bytes),
        }
        let count = bytes.len();
        reader.consume(count);
        processed += count as u64;
        progress(processed, total);
    }
    check_cancelled(cancelled)?;
    let after = reader
        .get_ref()
        .metadata()
        .map_err(|error| format!("não foi possível conferir a imagem: {error}"))?;
    if processed != total || after.len() != total || before.modified().ok() != after.modified().ok()
    {
        return Err("a imagem mudou durante o cálculo; selecione-a e calcule novamente".to_owned());
    }
    Ok(match algorithm {
        Algorithm::Sha256 => hex::encode(sha.finalize()),
        Algorithm::Md5 => hex::encode(md5.compute().0),
    })
}

#[tauri::command]
pub async fn compute_image_hash(
    app: AppHandle,
    manager: State<'_, HashManager>,
    path: String,
    algorithm: Option<String>,
    request_id: Option<String>,
) -> Result<String, String> {
    let algorithm = Algorithm::parse(algorithm.as_deref())?;
    let id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if id.is_empty() || id.len() > 128 {
        return Err("identificador de cálculo inválido".to_owned());
    }
    let job = manager.begin(id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let started = Instant::now();
        let mut last_event = None;
        let mut processed_bytes = 0;
        let result = hash_file(
            Path::new(&path),
            algorithm,
            &job.cancelled,
            |processed, total| {
                processed_bytes = processed;
                if last_event
                    .is_none_or(|last: Instant| last.elapsed() >= Duration::from_millis(100))
                {
                    let _ = app.emit(
                        "hash-progress",
                        HashProgress {
                            request_id: job.id.clone(),
                            // 100% apenas depois de conferir EOF e metadados.
                            percentage: if total == 0 {
                                0.0
                            } else {
                                (processed as f64 * 100.0 / total as f64).min(99.9)
                            },
                            bytes_processed: processed,
                            total_bytes: total,
                            bytes_per_second: processed as f64
                                / started.elapsed().as_secs_f64().max(0.001),
                        },
                    );
                    last_event = Some(Instant::now());
                }
            },
        );
        check_cancelled(&job.cancelled)?;
        if result.is_ok() {
            let _ = app.emit(
                "hash-progress",
                HashProgress {
                    request_id: job.id.clone(),
                    percentage: 100.0,
                    bytes_processed: processed_bytes,
                    total_bytes: processed_bytes,
                    bytes_per_second: processed_bytes as f64
                        / started.elapsed().as_secs_f64().max(0.001),
                },
            );
        }
        result
    })
    .await
    .map_err(|error| format!("falha no cálculo de hash: {error}"))?
}

#[tauri::command]
pub fn cancel_hash(
    manager: State<'_, HashManager>,
    request_id: Option<String>,
) -> Result<(), String> {
    if request_id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.len() > 128)
    {
        return Err("identificador de cálculo inválido".to_owned());
    }
    manager.cancel(request_id.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn computes_known_sha256_and_md5_vectors_on_disk() {
        for (bytes, sha, md5) in [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "d41d8cd98f00b204e9800998ecf8427e",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                "900150983cd24fb0d6963f7d28e17f72",
            ),
        ] {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            file.write_all(bytes).unwrap();
            for (algorithm, expected) in [(Algorithm::Sha256, sha), (Algorithm::Md5, md5)] {
                assert_eq!(
                    hash_file(file.path(), algorithm, &AtomicBool::new(false), |_, _| {}).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn hashes_multiple_buffers_and_cancels_before_the_remaining_bytes() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let bytes = vec![0x55; BUFFER_BYTES * 2 + 123];
        file.write_all(&bytes).unwrap();
        let sha = hash_file(
            file.path(),
            Algorithm::Sha256,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(sha, hex::encode(Sha256::digest(&bytes)));
        let cancelled = AtomicBool::new(false);
        let mut last = 0;
        let error = hash_file(
            file.path(),
            Algorithm::Sha256,
            &cancelled,
            |processed, _| {
                last = processed;
                if processed > 0 {
                    cancelled.store(true, Ordering::Release);
                }
            },
        )
        .unwrap_err();
        assert_eq!(error, CANCELLED);
        assert_eq!(last, BUFFER_BYTES as u64);
        file.close().unwrap(); // O descritor do leitor foi liberado, inclusive no Windows.
    }

    #[test]
    fn cancellation_and_old_job_cleanup_do_not_affect_a_new_request() {
        let manager = HashManager::default();
        manager.cancel(Some("before-start")).unwrap();
        assert!(manager.begin("before-start".to_owned()).is_err());
        let old = manager.begin("old".to_owned()).unwrap();
        let new = manager.begin("new".to_owned()).unwrap();
        assert!(check_cancelled(&old.cancelled).is_err());
        drop(old);
        manager.cancel(Some("old")).unwrap();
        assert!(check_cancelled(&new.cancelled).is_ok());
        manager.cancel(Some("new")).unwrap();
        assert!(check_cancelled(&new.cancelled).is_err());
        drop(new);
        assert!(manager.0.lock().unwrap().active.is_none());
    }

    #[test]
    fn rejects_invalid_algorithms_missing_files_and_changed_length() {
        assert!(Algorithm::parse(Some("sha1")).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(
            hash_file(
                &dir.path().join("missing"),
                Algorithm::Sha256,
                &AtomicBool::new(false),
                |_, _| {}
            )
            .is_err()
        );
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"abc").unwrap();
        let result = hash_file(
            file.path(),
            Algorithm::Sha256,
            &AtomicBool::new(false),
            |processed, _| {
                if processed > 0 {
                    file.as_file().set_len(0).unwrap();
                }
            },
        );
        assert!(result.unwrap_err().contains("mudou"));
    }
}
