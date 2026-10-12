mod cancellation;
use cancellation::{
    CANCELLED_MESSAGE, CancellableIo, CancellationToken, finish_io, is_cancelled_error,
};
mod clean_drive;
mod disk_layout;
mod image_stream;
mod platform;
pub mod protocol;
mod sector_io;
mod wim;
mod windows_iso;

use crate::elevation;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use image_stream::{ImageSource, copy_stream, verify_stream};
use meraki_flash_core::{UsbDevice, find_removable_device, validate_image_for_device};
use protocol::{ClientControlMessage, ControlAction, OperationKind, StartFormatRequest};
use protocol::{
    ElevatedFlashRequest, FlashPhase, FlashProgress, HelperEnvelope, ImageKind, StartFlashRequest,
    StartFlashResponse,
};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::net::TcpStream as StdTcpStream;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use uuid::Uuid;

pub const HELPER_FLAG: &str = "--meraki-flash-elevated-helper";
const PROGRESS_EVENT: &str = "flash-progress";
const BUFFER_SIZE: usize = 4 * 1024 * 1024;

#[derive(Default)]
pub struct FlashManager {
    cancellation: Mutex<Option<watch::Sender<bool>>>,
}

impl FlashManager {
    pub fn begin(&self) -> Result<watch::Receiver<bool>, String> {
        let mut active = self
            .cancellation
            .lock()
            .map_err(|_| "gerenciador indisponível")?;
        if active.is_some() {
            return Err("já existe uma gravação em andamento".to_owned());
        }
        let (sender, receiver) = watch::channel(false);
        *active = Some(sender);
        Ok(receiver)
    }

    pub fn cancel(&self) -> Result<(), String> {
        let active = self
            .cancellation
            .lock()
            .map_err(|_| "gerenciador indisponível")?;
        if let Some(sender) = active.as_ref() {
            sender.send_replace(true);
        }
        Ok(())
    }

    pub fn finish(&self) {
        if let Ok(mut active) = self.cancellation.lock() {
            *active = None;
        }
    }
}

pub async fn start_flash(
    app: AppHandle,
    manager: &FlashManager,
    request: StartFlashRequest,
) -> Result<StartFlashResponse, String> {
    let device_id = request.device_id.clone();
    let device = tauri::async_runtime::spawn_blocking(move || find_removable_device(&device_id))
        .await
        .map_err(|error| format!("falha ao consultar o dispositivo: {error}"))?
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            "o dispositivo selecionado não está mais conectado ou não é removível".to_owned()
        })?;

    if device.read_only {
        return Err("o dispositivo selecionado está protegido contra gravação".to_owned());
    }
    validate_image_kind(Path::new(&request.iso_path), request.image_kind)?;
    let (iso_path, iso_size) = validate_image_for_device(&request.iso_path, &device)?;
    let image_kind = request.image_kind;
    let unattend_xml_content =
        validate_unattend_for_image(image_kind, request.unattend_xml_content.as_deref())?;
    launch_operation(
        app,
        manager,
        device,
        Operation::Flash {
            iso_path: iso_path.to_string_lossy().into_owned(),
            iso_size,
            image_kind,
            unattend_xml_content,
        },
    )
}

enum Operation {
    Flash {
        iso_path: String,
        iso_size: u64,
        image_kind: ImageKind,
        unattend_xml_content: Option<String>,
    },
    Format {
        volume_label: String,
    },
}

pub async fn start_format_device(
    app: AppHandle,
    manager: &FlashManager,
    request: StartFormatRequest,
) -> Result<StartFlashResponse, String> {
    let volume_label = clean_drive::normalize_label(request.volume_label.as_deref())?;
    let device =
        tauri::async_runtime::spawn_blocking(move || find_removable_device(&request.device_id))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                "o dispositivo não está conectado ou foi bloqueado por segurança".to_owned()
            })?;
    if device.read_only {
        return Err("o dispositivo está protegido contra gravação".to_owned());
    }
    launch_operation(app, manager, device, Operation::Format { volume_label })
}

fn launch_operation(
    app: AppHandle,
    manager: &FlashManager,
    device: UsbDevice,
    operation: Operation,
) -> Result<StartFlashResponse, String> {
    let cancellation = manager.begin()?;

    let operation_id = Uuid::new_v4().to_string();
    let response = StartFlashResponse {
        operation_id: operation_id.clone(),
    };
    let app_for_task = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = run_flash_operation(
            &app_for_task,
            device,
            operation,
            operation_id.clone(),
            cancellation,
        )
        .await;

        app_for_task.state::<FlashManager>().finish();
        let terminal = result.unwrap_or_else(|message| {
            FlashProgress::new(
                operation_id,
                FlashPhase::Error,
                0.0,
                0.0,
                None,
                Some(message),
            )
        });
        emit_progress(&app_for_task, terminal);
    });

    Ok(response)
}

async fn run_flash_operation(
    app: &AppHandle,
    device: UsbDevice,
    operation: Operation,
    operation_id: String,
    cancellation: watch::Receiver<bool>,
) -> Result<FlashProgress, String> {
    emit_progress(
        app,
        FlashProgress::new(
            operation_id.clone(),
            FlashPhase::Preparing,
            0.0,
            0.0,
            None,
            Some("Aguardando autorização administrativa…".to_owned()),
        ),
    );

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("não foi possível abrir o canal de progresso: {error}"))?;
    let callback_port = listener
        .local_addr()
        .map_err(|error| format!("não foi possível configurar o canal de progresso: {error}"))?
        .port();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let (operation_kind, volume_label, iso_path, iso_size, image_kind, unattend_xml_content) =
        match operation {
            Operation::Flash {
                iso_path,
                iso_size,
                image_kind,
                unattend_xml_content,
            } => (
                OperationKind::Flash,
                None,
                iso_path,
                iso_size,
                image_kind,
                unattend_xml_content,
            ),
            Operation::Format { volume_label } => (
                OperationKind::Format,
                Some(volume_label),
                String::new(),
                0,
                ImageKind::Windows,
                None,
            ),
        };
    let helper_request = ElevatedFlashRequest {
        operation_kind,
        volume_label,
        operation_id: operation_id.clone(),
        token: token.clone(),
        callback_port,
        iso_path,
        iso_size,
        device_id: device.id,
        device_path: device.device_path,
        device_size: device.total_bytes,
        device_serial: device.serial,
        image_kind,
        unattend_xml_content,
    };
    let encoded = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&helper_request)
            .map_err(|error| format!("não foi possível preparar o helper: {error}"))?,
    );

    let mut elevation = Box::pin(elevation::launch_elevated(encoded));
    let mut connection = Box::pin(tokio::time::timeout(
        Duration::from_secs(120),
        accept_authenticated(&listener, &token, &operation_id),
    ));
    let (reader, first_progress) = tokio::select! {
        biased;
        accepted = &mut connection => accepted
            .map_err(|_| "a autenticação administrativa expirou".to_owned())??,
        elevated = &mut elevation => {
            elevated?;
            return Err("o helper terminou antes de abrir o canal de progresso".to_owned());
        }
    };

    let (read, write) = reader.into_inner().into_split();
    let forwarded = {
        let progress = forward_progress(
            app,
            BufReader::new(read),
            first_progress,
            &token,
            &operation_id,
        );
        tokio::pin!(progress);
        let control = forward_control(write, token.clone(), cancellation);
        tokio::pin!(control);
        tokio::select! {
            result = &mut progress => result,
            result = &mut control => {
                // A falha/fechamento da metade de escrita faz o helper abortar.
                let forwarded = progress.await;
                forwarded.map_err(|error| result.err().unwrap_or(error))
            }
        }
    };
    let helper_result = elevation.await;
    let terminal = forwarded?;
    match (helper_result, terminal.phase) {
        (Ok(0), FlashPhase::Done | FlashPhase::Cancelled) | (_, FlashPhase::Error) => Ok(terminal),
        (Err(error), _) => Err(error),
        (Ok(code), _) => Err(format!("o helper terminou com o código {code}")),
    }
}

async fn accept_authenticated(
    listener: &TcpListener,
    token: &str,
    operation_id: &str,
) -> Result<(BufReader<TcpStream>, FlashProgress), String> {
    loop {
        let (stream, address) = listener
            .accept()
            .await
            .map_err(|error| format!("falha no canal de progresso: {error}"))?;
        if !address.ip().is_loopback() {
            continue;
        }

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let bytes =
            match tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line)).await {
                Ok(Ok(bytes)) => bytes,
                _ => continue,
            };
        if bytes == 0 {
            continue;
        }
        if let Ok(envelope) = serde_json::from_str::<HelperEnvelope>(&line)
            && envelope.token == token
            && envelope.progress.operation_id == operation_id
        {
            return Ok((reader, envelope.progress));
        }
    }
}

async fn forward_control(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    token: String,
    mut cancellation: watch::Receiver<bool>,
) -> Result<(), String> {
    let cancelled = *cancellation.borrow();
    let mut message = serde_json::to_vec(&ClientControlMessage {
        token: token.clone(),
        action: if cancelled {
            ControlAction::Cancel
        } else {
            ControlAction::Start
        },
    })
    .map_err(|error| error.to_string())?;
    message.push(b'\n');
    writer
        .write_all(&message)
        .await
        .map_err(|error| error.to_string())?;
    if !cancelled {
        let _ = cancellation.wait_for(|cancelled| *cancelled).await;
        let mut message = serde_json::to_vec(&ClientControlMessage {
            token,
            action: ControlAction::Cancel,
        })
        .map_err(|error| error.to_string())?;
        message.push(b'\n');
        writer
            .write_all(&message)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn monitor_control(
    stream: StdTcpStream,
    token: &str,
    cancellation: &CancellationToken,
    ready: std::sync::mpsc::Sender<()>,
) {
    use std::io::{BufRead, Read};
    let mut reader = std::io::BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        // Um cliente inválido não pode aumentar indefinidamente a memória do helper.
        match reader.by_ref().take(4097).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) if line.len() > 4096 || !line.ends_with(b"\n") => break,
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_slice::<ClientControlMessage>(&line) else {
            break;
        };
        if message.token != token {
            continue;
        }
        if message.action == ControlAction::Cancel {
            cancellation.cancel();
            let _ = ready.send(());
            return;
        }
        let _ = ready.send(());
    }
    cancellation.cancel();
    let _ = ready.send(());
}

async fn forward_progress(
    app: &AppHandle,
    mut reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    first_progress: FlashProgress,
    token: &str,
    operation_id: &str,
) -> Result<FlashProgress, String> {
    let mut terminal = first_progress;
    if !terminal.phase.is_terminal() {
        emit_progress(app, terminal.clone());
    }

    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .await
            .map_err(|error| format!("o canal de progresso foi interrompido: {error}"))?;
        if bytes == 0 {
            break;
        }
        let envelope: HelperEnvelope = serde_json::from_str(&line)
            .map_err(|error| format!("mensagem inválida do helper: {error}"))?;
        if envelope.token != token || envelope.progress.operation_id != operation_id {
            return Err("o helper enviou uma mensagem não autenticada".to_owned());
        }
        terminal = envelope.progress;
        if !terminal.phase.is_terminal() {
            emit_progress(app, terminal.clone());
        }
    }
    if !terminal.phase.is_terminal() {
        return Err("o helper encerrou sem confirmar a finalização".to_owned());
    }
    Ok(terminal)
}

fn emit_progress(app: &AppHandle, progress: FlashProgress) {
    let _ = app.emit(PROGRESS_EVENT, progress);
}

pub fn run_elevated_helper(encoded_request: &str) -> i32 {
    let request = match URL_SAFE_NO_PAD
        .decode(encoded_request)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<ElevatedFlashRequest>(&bytes).map_err(|e| e.to_string())
        }) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("solicitação inválida para o helper: {error}");
            return 2;
        }
    };

    let mut reporter = match Reporter::connect(&request) {
        Ok(reporter) => reporter,
        Err(error) => {
            eprintln!("não foi possível conectar ao aplicativo: {error}");
            return 3;
        }
    };
    if let Err(error) = execute_elevated_flash(&request, &mut reporter) {
        let cancelled = is_cancelled_error(&error);
        let percentage = reporter.last_percentage();
        let _ = reporter.send(FlashProgress::new(
            request.operation_id.clone(),
            if cancelled {
                FlashPhase::Cancelled
            } else {
                FlashPhase::Error
            },
            percentage,
            0.0,
            None,
            Some(if cancelled {
                if request.operation_kind == OperationKind::Format {
                    "A formatação foi cancelada. O pendrive foi liberado; formate-o novamente antes de usar.".to_owned()
                } else { CANCELLED_MESSAGE.to_owned() }
            } else {
                error
            }),
        ));
        return if cancelled { 0 } else { 1 };
    }
    0
}

fn execute_elevated_flash(
    request: &ElevatedFlashRequest,
    reporter: &mut Reporter,
) -> Result<(), String> {
    if !platform::is_elevated() {
        return Err("o helper não recebeu privilégios administrativos".to_owned());
    }
    reporter.send(FlashProgress::new(
        request.operation_id.clone(),
        FlashPhase::Preparing,
        0.0,
        0.0,
        None,
        Some("Validando novamente o dispositivo…".to_owned()),
    ))?;

    reporter.cancellation.check()?;
    let device = revalidate_device(request)?;

    if request.operation_kind == OperationKind::Format {
        clean_drive::execute(
            &device,
            request.volume_label.as_deref(),
            &request.operation_id,
            reporter,
        )?;
        reporter.cancellation.check()?;
        reporter.send(FlashProgress::new(
            &request.operation_id,
            FlashPhase::Done,
            100.0,
            0.0,
            Some(0),
            Some(
                "Pendrive restaurado e formatado com sucesso! Já está pronto para uso comum."
                    .to_owned(),
            ),
        ))?;
        return Ok(());
    }

    validate_image_kind(Path::new(&request.iso_path), request.image_kind)?;
    let (iso_path, iso_size) = validate_image_for_device(&request.iso_path, &device)?;
    if iso_size != request.iso_size {
        return Err("o tamanho da imagem mudou depois da confirmação".to_owned());
    }
    let unattend_xml_content =
        validate_unattend_for_image(request.image_kind, request.unattend_xml_content.as_deref())?;

    match request.image_kind {
        ImageKind::Linux => {
            let mut source = ImageSource::open(&iso_path, device.total_bytes)?;
            source.validate_header()?;
            reporter.send(FlashProgress::new(
                request.operation_id.clone(),
                FlashPhase::Preparing,
                0.0,
                0.0,
                None,
                Some("Desmontando e bloqueando o dispositivo…".to_owned()),
            ))?;
            reporter.cancellation.check()?;
            let mut prepared = platform::prepare_device(&device)?;
            let sector_size = platform::logical_sector_size(&prepared.file)?;
            write_image(
                source,
                &mut prepared.file,
                (sector_size, device.total_bytes),
                &request.operation_id,
                reporter,
            )?;
            drop(prepared);
        }
        ImageKind::Windows => {
            let source = File::open(&iso_path)
                .map_err(|error| format!("não foi possível abrir a ISO: {error}"))?;
            let image = windows_iso::prepare(
                source,
                &iso_path,
                &device,
                unattend_xml_content,
                &request.operation_id,
                reporter,
            )?;
            // Dividir WIMs grandes pode levar minutos. A identidade física é
            // conferida novamente imediatamente antes da primeira escrita.
            reporter.cancellation.check()?;
            let device = revalidate_device(request)?;
            windows_iso::write(image, &device, &request.operation_id, reporter)?;
        }
    }

    reporter.cancellation.check()?;
    let _ = reporter.send(FlashProgress::new(
        request.operation_id.clone(),
        FlashPhase::Done,
        100.0,
        0.0,
        Some(0),
        Some("Pendrive gravado com sucesso. Já é seguro removê-lo.".to_owned()),
    ));
    Ok(())
}

fn validate_unattend_for_image(
    image_kind: ImageKind,
    content: Option<&str>,
) -> Result<Option<String>, String> {
    match (image_kind, content) {
        (ImageKind::Linux, Some(_)) => {
            Err("autounattend.xml só pode ser usado com imagens do Windows".to_owned())
        }
        (ImageKind::Linux, None) => Ok(None),
        (ImageKind::Windows, content) => windows_iso::validate_unattend_xml(content),
    }
}

fn revalidate_device(request: &ElevatedFlashRequest) -> Result<UsbDevice, String> {
    let device = find_removable_device(&request.device_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "o dispositivo removível não está mais conectado".to_owned())?;
    if device.device_path != request.device_path
        || device.total_bytes != request.device_size
        || device.serial != request.device_serial
    {
        return Err(
            "a identidade física do dispositivo mudou; a operação foi cancelada".to_owned(),
        );
    }
    if device.read_only {
        return Err("o dispositivo está protegido contra gravação".to_owned());
    }
    Ok(device)
}

fn write_image(
    source: ImageSource,
    target_file: &mut File,
    geometry: (u32, u64),
    operation_id: &str,
    reporter: &mut Reporter,
) -> Result<(), String> {
    let result = write_image_inner(source, target_file, geometry, operation_id, reporter);
    finish_io(result, target_file.sync_all())
}

fn write_image_inner(
    mut source: ImageSource,
    target_file: &mut File,
    geometry: (u32, u64),
    operation_id: &str,
    reporter: &mut Reporter,
) -> Result<(), String> {
    let mut target = sector_io::SectorIo::new(&mut *target_file, geometry.0, geometry.1)
        .map_err(|error| error.to_string())?;
    target
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let mut last_report = Instant::now();
    let expected = source.expanded_size;
    let cancellation = reporter.cancellation.clone();
    cancellation.check()?;
    let _ = reporter.send(FlashProgress::new(
        operation_id,
        FlashPhase::Writing,
        0.0,
        0.0,
        None,
        Some("Gravando a imagem em streaming…".to_owned()),
    ));
    let written = source.with_reader(|reader, progress| {
        copy_stream(
            &mut CancellableIo::new(reader, &cancellation),
            &mut CancellableIo::new(&mut target, &cancellation),
            geometry.1,
            expected,
            |written| {
                if last_report.elapsed() >= Duration::from_millis(250) {
                    let elapsed = started.elapsed().as_secs_f64().max(0.001);
                    let fraction = expected
                        .map_or_else(|| progress.fraction(), |size| written as f64 / size as f64);
                    // O decoder pode antecipar a leitura do arquivo. Só conclui após EOF/CRC.
                    let percentage = fraction.min(0.999) * 90.0;
                    let eta = (fraction > 0.0 && fraction < 1.0)
                        .then(|| (elapsed * (1.0 - fraction) / fraction).ceil() as u64);
                    let _ = reporter.send(FlashProgress::new(
                        operation_id,
                        FlashPhase::Writing,
                        percentage,
                        written as f64 / elapsed,
                        eta,
                        None,
                    ));
                    last_report = Instant::now();
                }
            },
        )
    })?;
    let _ = reporter.send(FlashProgress::new(
        operation_id,
        FlashPhase::Writing,
        90.0,
        written as f64 / started.elapsed().as_secs_f64().max(0.001),
        Some(0),
        None,
    ));
    let _ = reporter.send(FlashProgress::new(
        operation_id,
        FlashPhase::Syncing,
        90.0,
        0.0,
        None,
        Some("Sincronizando os dados com o dispositivo…".to_owned()),
    ));
    let _ = target.finish().map_err(|error| error.to_string())?;
    target_file
        .sync_all()
        .map_err(|error| format!("falha ao sincronizar o dispositivo: {error}"))?;
    let mut target = sector_io::SectorIo::new(target_file, geometry.0, geometry.1)
        .map_err(|error| error.to_string())?;
    let _ = reporter.send(FlashProgress::new(
        operation_id,
        FlashPhase::Verifying,
        90.0,
        0.0,
        None,
        Some("Conferindo os dados gravados…".to_owned()),
    ));
    source.with_reader(|reader, _| {
        verify_stream(
            &mut CancellableIo::new(reader, &cancellation),
            &mut CancellableIo::new(&mut target, &cancellation),
            written,
            |verified| {
                if last_report.elapsed() >= Duration::from_millis(250) {
                    let _ = reporter.send(FlashProgress::new(
                        operation_id,
                        FlashPhase::Verifying,
                        (90.0 + verified as f64 * 10.0 / written as f64).min(99.9),
                        0.0,
                        None,
                        None,
                    ));
                    last_report = Instant::now();
                }
            },
        )
    })?;
    cancellation.check()?;
    let _ = reporter.send(FlashProgress::new(
        operation_id,
        FlashPhase::Verifying,
        100.0,
        0.0,
        Some(0),
        Some("Verificação concluída.".to_owned()),
    ));
    Ok(())
}

struct Reporter {
    stream: StdTcpStream,
    token: String,
    last_percentage: f64,
    cancellation: CancellationToken,
    monitor: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Reporter {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

impl Reporter {
    fn connect(request: &ElevatedFlashRequest) -> Result<Self, String> {
        let stream = StdTcpStream::connect_timeout(
            &format!("127.0.0.1:{}", request.callback_port)
                .parse()
                .map_err(|error| format!("endereço de retorno inválido: {error}"))?,
            Duration::from_secs(10),
        )
        .map_err(|error| format!("falha ao abrir o canal de progresso: {error}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| format!("falha ao configurar o canal de progresso: {error}"))?;
        let cancellation = CancellationToken::default();
        let control_stream = stream.try_clone().map_err(|error| error.to_string())?;
        let control_token = request.token.clone();
        let monitor_token = cancellation.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let monitor = std::thread::spawn(move || {
            monitor_control(control_stream, &control_token, &monitor_token, ready_tx)
        });
        let mut reporter = Self {
            stream,
            token: request.token.clone(),
            last_percentage: 0.0,
            cancellation,
            monitor: Some(monitor),
        };
        reporter.send(FlashProgress::new(
            &request.operation_id,
            FlashPhase::Preparing,
            0.0,
            0.0,
            None,
            Some("Conectando ao aplicativo…".to_owned()),
        ))?;
        // Nenhuma operação destrutiva começa sem autorização autenticada do pai.
        if ready_rx.recv_timeout(Duration::from_secs(120)).is_err() {
            reporter.cancellation.cancel();
        }
        Ok(reporter)
    }

    fn send(&mut self, progress: FlashProgress) -> Result<(), String> {
        self.last_percentage = progress.percentage;
        let result = (|| {
            serde_json::to_writer(
                &mut self.stream,
                &HelperEnvelope {
                    token: self.token.clone(),
                    progress,
                },
            )
            .map_err(|error| format!("falha ao serializar o progresso: {error}"))?;
            self.stream
                .write_all(b"\n")
                .and_then(|_| self.stream.flush())
                .map_err(|error| format!("falha ao enviar o progresso: {error}"))
        })();
        if result.is_err() {
            self.cancellation.cancel();
        }
        result
    }

    fn last_percentage(&self) -> f64 {
        self.last_percentage
    }
}

fn validate_image_kind(path: &Path, kind: ImageKind) -> Result<(), String> {
    if kind == ImageKind::Windows
        && !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("iso") || extension.eq_ignore_ascii_case("img")
            })
    {
        return Err("para Windows, selecione uma ISO (.iso ou .img) descompactada com estrutura ISO9660/UDF; arquivos compactados são aceitos apenas no modo Linux".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ElevatedFlashRequest, FlashPhase, HelperEnvelope, ImageKind, Reporter, write_image,
    };
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::net::TcpListener;
    use std::thread;
    use tempfile::{Builder, NamedTempFile};

    #[test]
    fn cancellation_is_idempotent_and_does_not_leak_to_the_next_operation() {
        let manager = super::FlashManager::default();
        let first = manager.begin().unwrap();
        assert!(manager.begin().is_err());
        manager.cancel().unwrap();
        manager.cancel().unwrap();
        assert!(*first.borrow());
        manager.finish();
        assert!(!*manager.begin().unwrap().borrow());
        manager.finish();
        manager.cancel().unwrap();
    }

    #[test]
    fn authenticates_control_and_cancels_on_disconnect_or_cancel() {
        for disconnect in [true, false] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let token = super::CancellationToken::default();
            let monitored = token.clone();
            let (sender, ready) = std::sync::mpsc::channel();
            let thread =
                thread::spawn(move || super::monitor_control(server, "secret", &monitored, sender));
            client.write_all(b"{\"token\":\"wrong\",\"action\":\"cancel\"}\n{\"token\":\"secret\",\"action\":\"start\"}\n").unwrap();
            ready
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert!(!token.is_cancelled());
            if disconnect {
                drop(client);
            } else {
                client
                    .write_all(b"{\"token\":\"secret\",\"action\":\"cancel\"}\n")
                    .unwrap();
            }
            thread.join().unwrap();
            assert!(token.is_cancelled());
        }
    }

    #[test]
    fn queued_cancel_prevents_start_and_oversized_frames_abort() {
        for frame in [
            b"{\"token\":\"secret\",\"action\":\"cancel\"}\n".to_vec(),
            vec![b'x'; 4097],
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let token = super::CancellationToken::default();
            let monitored = token.clone();
            let (sender, ready) = std::sync::mpsc::channel();
            let thread =
                thread::spawn(move || super::monitor_control(server, "secret", &monitored, sender));
            client.write_all(&frame).unwrap();
            ready
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            thread.join().unwrap();
            assert!(token.is_cancelled());
        }
    }

    #[test]
    fn windows_requires_uncompressed_iso_or_img() {
        for path in ["windows.iso", "windows.IMG"] {
            super::validate_image_kind(std::path::Path::new(path), ImageKind::Windows).unwrap();
        }
        for path in [
            "windows.iso.gz",
            "windows.xz",
            "windows.zip",
            "disk.raw",
            "disk.bin",
        ] {
            assert!(
                super::validate_image_kind(std::path::Path::new(path), ImageKind::Windows)
                    .unwrap_err()
                    .contains("ISO")
            );
            super::validate_image_kind(std::path::Path::new(path), ImageKind::Linux).unwrap();
        }
    }

    #[test]
    fn copies_an_image_exactly_and_reports_completion() {
        let payload = (0..(1024 * 1024 + 137))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut source = Builder::new().suffix(".iso").tempfile().unwrap();
        source.write_all(&payload).unwrap();
        source.flush().unwrap();
        let mut target = NamedTempFile::new().unwrap();
        target
            .as_file_mut()
            .set_len((payload.len() as u64).div_ceil(512) * 512)
            .unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let callback_port = listener.local_addr().unwrap().port();
        let reader = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"{\"token\":\"test-token\",\"action\":\"start\"}\n")
                .unwrap();
            let mut messages = String::new();
            stream.read_to_string(&mut messages).unwrap();
            messages
        });
        let request = ElevatedFlashRequest {
            operation_kind: super::OperationKind::Flash,
            volume_label: None,
            operation_id: "test-operation".to_owned(),
            token: "test-token".to_owned(),
            callback_port,
            iso_path: source.path().to_string_lossy().into_owned(),
            iso_size: payload.len() as u64,
            device_id: "test-device".to_owned(),
            device_path: "test-target".to_owned(),
            device_size: payload.len() as u64,
            device_serial: None,
            image_kind: ImageKind::Linux,
            unattend_xml_content: None,
        };
        let mut reporter = Reporter::connect(&request).unwrap();
        let source_file =
            super::ImageSource::open(source.path(), (payload.len() as u64).div_ceil(512) * 512)
                .unwrap();
        let mut target_file = target.reopen().unwrap();

        write_image(
            source_file,
            &mut target_file,
            (512, (payload.len() as u64).div_ceil(512) * 512),
            &request.operation_id,
            &mut reporter,
        )
        .unwrap();
        drop(reporter);

        target_file.seek(SeekFrom::Start(0)).unwrap();
        let mut copied = Vec::new();
        target_file.read_to_end(&mut copied).unwrap();
        assert_eq!(&copied[..payload.len()], payload.as_slice());
        assert!(copied[payload.len()..].iter().all(|byte| *byte == 0));

        let messages = reader.join().unwrap();
        let envelopes = messages
            .lines()
            .map(|line| serde_json::from_str::<HelperEnvelope>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(
            envelopes
                .iter()
                .any(|event| event.progress.phase == FlashPhase::Verifying
                    && event.progress.percentage == 100.0)
        );
        assert!(envelopes.iter().any(|event| {
            event.progress.phase == FlashPhase::Writing && event.progress.percentage == 90.0
        }));
        assert!(
            envelopes
                .iter()
                .any(|event| event.progress.phase == FlashPhase::Syncing)
        );
    }
}
