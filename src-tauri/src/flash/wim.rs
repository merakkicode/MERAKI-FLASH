use super::cancellation::CancellationToken;
use std::path::Path;

/// Exporta todos os índices de um ESD sólido antes de dividir a imagem.
/// O intermediário fica no mesmo workspace dos SWMs e é removido em sucesso/erro.
#[cfg(test)]
pub fn split_or_export_image(
    input_path: &Path,
    output_path: &Path,
    is_solid_esd: bool,
    part_bytes: u64,
) -> Result<(), String> {
    split_or_export_image_cancellable(
        input_path,
        output_path,
        is_solid_esd,
        part_bytes,
        &CancellationToken::default(),
    )
}

pub fn split_or_export_image_cancellable(
    input_path: &Path,
    output_path: &Path,
    is_solid_esd: bool,
    part_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    cancellation.check()?;
    if part_bytes == 0 || part_bytes > i64::MAX as u64 {
        return Err("o tamanho solicitado para o SWM é inválido".to_owned());
    }
    if !is_solid_esd {
        let result = split_cancellable(input_path, output_path, part_bytes, cancellation);
        cancellation.check()?;
        return result;
    }
    let parent = output_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::Builder::new()
        .prefix("esd-export-")
        .tempdir_in(parent)
        .map_err(|error| format!("não foi possível criar o workspace de conversão ESD: {error}"))?;
    let intermediate = temporary.path().join("converted.wim");
    let exported = export_esd_cancellable(input_path, &intermediate, cancellation);
    cancellation.check()?;
    exported?;
    // A exportação fecha todos os handles antes de reabrir o WIM para divisão.
    let split = split_cancellable(&intermediate, output_path, part_bytes, cancellation);
    cancellation.check()?;
    split?;
    temporary
        .close()
        .map_err(|error| format!("não foi possível liberar o WIM intermediário: {error}"))
}

#[cfg(target_os = "linux")]
use wimlib_sys as wim_sys;

#[cfg(target_os = "linux")]
static WIM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "linux")]
fn register_cancellation(image: *mut wim_sys::WIMStruct, token: &CancellationToken) {
    unsafe extern "C" fn callback(
        _: wim_sys::wimlib_progress_msg,
        _: *mut wim_sys::wimlib_progress_info,
        context: *mut std::ffi::c_void,
    ) -> wim_sys::wimlib_progress_status {
        // SAFETY: o token vive até depois de liberar o WIM que usa este callback.
        let token = unsafe { &*context.cast::<CancellationToken>() };
        if token.is_cancelled() { 1 } else { 0 }
    }
    // SAFETY: o handle é válido e a referência permanece viva durante a operação.
    unsafe {
        wim_sys::wimlib_register_progress_function(
            image,
            Some(callback),
            std::ptr::from_ref(token).cast_mut().cast(),
        )
    };
}

#[cfg(target_os = "windows")]
use windows::{export_esd_cancellable, split_cancellable};

#[cfg(target_os = "linux")]
fn split_cancellable(
    input_path: &Path,
    output_path: &Path,
    part_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;

    let mut input = input_path.as_os_str().as_bytes().to_vec();
    input.push(0);
    let mut output = output_path.as_os_str().as_bytes().to_vec();
    output.push(0);
    perform_split(
        input.as_ptr().cast(),
        output.as_ptr().cast(),
        part_bytes,
        cancellation,
    )
}

#[cfg(target_os = "linux")]
fn perform_split(
    input: *const wim_sys::wimlib_tchar,
    output: *const wim_sys::wimlib_tchar,
    part_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let _lock = WIM_LOCK
        .lock()
        .map_err(|_| "o estado da wimlib foi interrompido".to_owned())?;
    // SAFETY: esta operação é serializada pelo FlashManager. Os caminhos são
    // strings terminadas em NUL e permanecem vivos até a função retornar.
    let init_status = unsafe { wim_sys::wimlib_global_init(0) };
    if init_status != 0 {
        return Err(format!(
            "não foi possível inicializar wimlib: {}",
            error_message(init_status)
        ));
    }
    let _global = WimGlobalGuard;

    let mut image = std::ptr::null_mut();
    // SAFETY: `input` é válido e `image` aponta para memória gravável.
    let open_status = unsafe { wim_sys::wimlib_open_wim(input, 0, &mut image) };
    if open_status != 0 {
        return Err(format!(
            "não foi possível abrir install.wim: {}",
            error_message(open_status)
        ));
    }
    let image = WimImageGuard(image);
    register_cancellation(image.0, cancellation);
    cancellation.check()?;
    // SAFETY: o WIM foi aberto com sucesso e `output` é uma string válida.
    let split_status = unsafe { wim_sys::wimlib_split(image.0, output, part_bytes, 0) };
    cancellation.check()?;
    if split_status != 0 {
        let system_detail = relevant_write_error()
            .map(|error| format!("; sistema: {error}"))
            .unwrap_or_default();
        return Err(format!(
            "não foi possível dividir install.wim (wimlib {split_status}): {}{system_detail}",
            error_message(split_status),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn export_esd_cancellable(
    input_path: &Path,
    output_path: &Path,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let input = std::ffi::CString::new(input_path.as_os_str().as_bytes())
        .map_err(|_| "caminho ESD inválido".to_owned())?;
    let output = std::ffi::CString::new(output_path.as_os_str().as_bytes())
        .map_err(|_| "caminho WIM inválido".to_owned())?;
    let _lock = WIM_LOCK
        .lock()
        .map_err(|_| "o estado da wimlib foi interrompido".to_owned())?;
    // SAFETY: inicialização serializada; os guards são destruídos antes do lock.
    check_wim_status(
        unsafe { wim_sys::wimlib_global_init(0) },
        "inicializar wimlib",
    )?;
    let _global = WimGlobalGuard;
    let mut source = std::ptr::null_mut();
    // SAFETY: caminho terminado em NUL e ponteiro de saída gravável.
    check_wim_status(
        unsafe { wim_sys::wimlib_open_wim(input.as_ptr(), 0, &mut source) },
        "abrir install.esd",
    )?;
    let source = WimImageGuard(source);
    register_cancellation(source.0, cancellation);
    cancellation.check()?;
    let mut destination = std::ptr::null_mut();
    // SAFETY: destination é válido para receber um novo WIM sem compressão sólida.
    check_wim_status(
        unsafe {
            wim_sys::wimlib_create_new_wim(
                wim_sys::wimlib_compression_type_WIMLIB_COMPRESSION_TYPE_NONE,
                &mut destination,
            )
        },
        "criar o WIM intermediário",
    )?;
    let destination = WimImageGuard(destination);
    register_cancellation(destination.0, cancellation);
    // SAFETY: ambos os WIMs permanecem vivos, inclusive durante wimlib_write.
    // Nomes/descrições nulos preservam os metadados de todos os índices.
    check_wim_status(
        unsafe {
            wim_sys::wimlib_export_image(
                source.0,
                wim_sys::WIMLIB_ALL_IMAGES,
                destination.0,
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        },
        "exportar os índices de install.esd",
    )?;
    // SAFETY: gravação materializa os recursos exportados antes do split.
    // RECOMPRESS e ausência de SOLID impedem reaproveitar recursos sólidos.
    check_wim_status(
        unsafe {
            wim_sys::wimlib_write(
                destination.0,
                output.as_ptr(),
                wim_sys::WIMLIB_ALL_IMAGES,
                wim_sys::WIMLIB_WRITE_FLAG_RECOMPRESS as i32,
                1,
            )
        },
        "gravar o WIM intermediário",
    )
}

#[cfg(target_os = "linux")]
fn check_wim_status(status: i32, action: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        let detail = relevant_write_error()
            .map(|error| format!("; sistema: {error}"))
            .unwrap_or_default();
        Err(format!(
            "não foi possível {action}: {} (wimlib {status}){detail}",
            error_message(status)
        ))
    }
}

#[cfg(target_os = "linux")]
struct WimGlobalGuard;

#[cfg(target_os = "linux")]
impl Drop for WimGlobalGuard {
    fn drop(&mut self) {
        // SAFETY: corresponde à inicialização bem-sucedida desta thread.
        unsafe { wim_sys::wimlib_global_cleanup() };
    }
}

#[cfg(target_os = "linux")]
struct WimImageGuard(*mut wim_sys::WIMStruct);

#[cfg(target_os = "linux")]
impl Drop for WimImageGuard {
    fn drop(&mut self) {
        // SAFETY: o ponteiro foi retornado por wimlib_open_wim e é liberado uma vez.
        unsafe { wim_sys::wimlib_free(self.0) };
    }
}

#[cfg(target_os = "linux")]
fn error_message(code: i32) -> String {
    // SAFETY: wimlib retorna uma string estática terminada em NUL.
    let pointer = unsafe { wim_sys::wimlib_get_error_string(code as _) };
    if pointer.is_null() {
        return format!("erro {code}");
    }
    // SAFETY: validade garantida pelo contrato de wimlib_get_error_string.
    unsafe { std::ffi::CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(target_os = "linux")]
fn relevant_write_error() -> Option<String> {
    let error = std::io::Error::last_os_error();
    let code = error.raw_os_error()?;
    [
        libc::ENOSPC,
        libc::EDQUOT,
        libc::EFBIG,
        libc::EACCES,
        libc::EROFS,
        libc::EIO,
    ]
    .contains(&code)
    .then(|| format!("{error} (errno {code})"))
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        FreeLibrary, GENERIC_READ, GENERIC_WRITE, HANDLE, HMODULE,
    };
    use windows_sys::Win32::Storage::FileSystem::{CREATE_ALWAYS, OPEN_EXISTING};
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };

    type WimCreateFile =
        unsafe extern "system" fn(*const u16, u32, u32, u32, u32, *mut u32) -> HANDLE;
    type WimSplitFile = unsafe extern "system" fn(HANDLE, *const u16, *mut i64, u32) -> i32;
    type WimCloseHandle = unsafe extern "system" fn(HANDLE) -> i32;
    type WimGetImageCount = unsafe extern "system" fn(HANDLE) -> u32;
    type WimLoadImage = unsafe extern "system" fn(HANDLE, u32) -> HANDLE;
    type WimExportImage = unsafe extern "system" fn(HANDLE, HANDLE, u32) -> i32;
    type WimSetTemporaryPath = unsafe extern "system" fn(HANDLE, *const u16) -> i32;
    type WimMessageCallback =
        unsafe extern "system" fn(u32, usize, isize, *mut std::ffi::c_void) -> u32;
    type WimRegisterCallback =
        unsafe extern "system" fn(HANDLE, WimMessageCallback, *mut std::ffi::c_void) -> u32;
    type WimUnregisterCallback = unsafe extern "system" fn(HANDLE, WimMessageCallback) -> i32;

    unsafe extern "system" fn cancellation_callback(
        message: u32,
        _: usize,
        _: isize,
        context: *mut std::ffi::c_void,
    ) -> u32 {
        // WIM_MSG_PROCESS e WIM_MSG_QUERY_ABORT aceitam WIM_MSG_ABORT_IMAGE.
        // SAFETY: CallbackGuard mantém o token vivo até remover o callback.
        let token = unsafe { &*context.cast::<CancellationToken>() };
        if matches!(message, 0x9479 | 0x948e) && token.is_cancelled() {
            u32::MAX
        } else {
            0
        }
    }

    struct CallbackGuard<'a> {
        api: &'a WimApi,
        handle: &'a WimHandle,
        _token: Box<CancellationToken>,
    }
    impl Drop for CallbackGuard<'_> {
        fn drop(&mut self) {
            // SAFETY: DLL, handle e userdata continuam vivos durante a remoção.
            unsafe { (self.api.unregister_callback)(self.handle.raw, cancellation_callback) };
        }
    }

    const WIM_COMPRESS_NONE: u32 = 0;

    struct WimApi {
        module: HMODULE,
        create_file: WimCreateFile,
        split_file: WimSplitFile,
        close_handle: WimCloseHandle,
        get_image_count: WimGetImageCount,
        load_image: WimLoadImage,
        export_image: WimExportImage,
        set_temporary_path: WimSetTemporaryPath,
        register_callback: WimRegisterCallback,
        unregister_callback: WimUnregisterCallback,
    }

    impl WimApi {
        fn load() -> Result<Self, String> {
            let library = "wimgapi.dll"
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<_>>();
            // SAFETY: o nome é UTF-16 terminado em NUL e restringimos a busca
            // ao System32 para impedir carregamento de DLL a partir do projeto.
            let module = unsafe {
                LoadLibraryExW(
                    library.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            };
            if module.is_null() {
                return Err(format!(
                    "não foi possível carregar a API de imagens do Windows: {}",
                    std::io::Error::last_os_error()
                ));
            }

            macro_rules! load_function {
                ($name:literal, $kind:ty) => {{
                    // SAFETY: `module` permanece carregado durante toda a vida
                    // de WimApi e cada assinatura corresponde a wimgapi.h.
                    let function = unsafe { GetProcAddress(module, concat!($name, "\0").as_ptr()) };
                    let Some(function) = function else {
                        // SAFETY: nenhum ponteiro de função foi usado ainda.
                        unsafe { FreeLibrary(module) };
                        return Err(format!(
                            "a função {} não está disponível em wimgapi.dll",
                            $name
                        ));
                    };
                    // SAFETY: a assinatura concreta acima corresponde à
                    // função exportada com convenção de chamada `system`.
                    unsafe {
                        std::mem::transmute::<unsafe extern "system" fn() -> isize, $kind>(function)
                    }
                }};
            }

            Ok(Self {
                module,
                create_file: load_function!("WIMCreateFile", WimCreateFile),
                split_file: load_function!("WIMSplitFile", WimSplitFile),
                close_handle: load_function!("WIMCloseHandle", WimCloseHandle),
                get_image_count: load_function!("WIMGetImageCount", WimGetImageCount),
                load_image: load_function!("WIMLoadImage", WimLoadImage),
                export_image: load_function!("WIMExportImage", WimExportImage),
                set_temporary_path: load_function!("WIMSetTemporaryPath", WimSetTemporaryPath),
                register_callback: load_function!(
                    "WIMRegisterMessageCallback",
                    WimRegisterCallback
                ),
                unregister_callback: load_function!(
                    "WIMUnregisterMessageCallback",
                    WimUnregisterCallback
                ),
            })
        }

        fn register<'a>(
            &'a self,
            handle: &'a WimHandle,
            token: &CancellationToken,
        ) -> Result<CallbackGuard<'a>, String> {
            let mut token = Box::new(token.clone());
            // SAFETY: o Box tem endereço estável e fica retido pelo guard.
            let index = unsafe {
                (self.register_callback)(
                    handle.raw,
                    cancellation_callback,
                    std::ptr::from_mut(token.as_mut()).cast(),
                )
            };
            if index == u32::MAX {
                return Err("não foi possível registrar o cancelamento WIMGAPI".to_owned());
            }
            Ok(CallbackGuard {
                api: self,
                handle,
                _token: token,
            })
        }

        fn open(&self, path: &Path, access: u32, disposition: u32) -> Result<WimHandle, String> {
            let path_wide = wide_path(path);
            // SAFETY: caminho válido; a API permanece carregada até fechar o handle.
            let raw = unsafe {
                (self.create_file)(
                    path_wide.as_ptr(),
                    access,
                    disposition,
                    0,
                    WIM_COMPRESS_NONE,
                    std::ptr::null_mut(),
                )
            };
            if raw.is_null() {
                return Err(format!(
                    "não foi possível abrir {} com WIMGAPI: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            Ok(WimHandle {
                raw,
                close: self.close_handle,
            })
        }

        fn set_workspace(&self, handle: &WimHandle, directory: &Path) -> Result<(), String> {
            let directory = wide_path(directory);
            // SAFETY: handle aberto e caminho válido durante a chamada.
            if unsafe { (self.set_temporary_path)(handle.raw, directory.as_ptr()) } == 0 {
                return Err(format!(
                    "não foi possível configurar o workspace WIMGAPI: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
    }

    impl Drop for WimApi {
        fn drop(&mut self) {
            // SAFETY: o módulo foi carregado uma vez por WimApi::load.
            unsafe { FreeLibrary(self.module) };
        }
    }

    struct WimHandle {
        raw: HANDLE,
        close: WimCloseHandle,
    }

    impl Drop for WimHandle {
        fn drop(&mut self) {
            // SAFETY: o handle foi retornado por WIMCreateFile e é fechado uma vez.
            unsafe { (self.close)(self.raw) };
        }
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    pub(super) fn export_esd_cancellable(
        input_path: &Path,
        output_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        cancellation.check()?;
        let api = WimApi::load()?;
        let source = api.open(input_path, GENERIC_READ, OPEN_EXISTING)?;
        let _source_callback = api.register(&source, cancellation)?;
        let destination = api.open(output_path, GENERIC_READ | GENERIC_WRITE, CREATE_ALWAYS)?;
        let _destination_callback = api.register(&destination, cancellation)?;
        let workspace = output_path
            .parent()
            .ok_or_else(|| "workspace ESD inválido".to_owned())?;
        api.set_workspace(&source, workspace)?;
        api.set_workspace(&destination, workspace)?;
        // SAFETY: handle da origem aberto e válido.
        let count = unsafe { (api.get_image_count)(source.raw) };
        if count == 0 {
            return Err("install.esd não contém índices de imagem exportáveis".to_owned());
        }
        for index in 1..=count {
            cancellation.check()?;
            // SAFETY: índice 1-based dentro do total de imagens da origem.
            let raw = unsafe { (api.load_image)(source.raw, index) };
            if raw.is_null() {
                return Err(format!(
                    "não foi possível carregar o índice {index} de install.esd: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let image = WimHandle {
                raw,
                close: api.close_handle,
            };
            // SAFETY: handles abertos, destino gravável e scratch configurado.
            let exported = unsafe { (api.export_image)(image.raw, destination.raw, 0) };
            cancellation.check()?;
            if exported == 0 {
                return Err(format!(
                    "não foi possível exportar o índice {index} de install.esd: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        // A ordem RAII fecha imagens, destino e origem antes de descarregar a DLL.
        Ok(())
    }

    pub(super) fn split_cancellable(
        input_path: &Path,
        output_path: &Path,
        part_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        cancellation.check()?;
        let api = WimApi::load()?;
        let input = input_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let output = output_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // SAFETY: os caminhos são UTF-16 terminados em NUL; os flags abrem um WIM
        // existente somente para leitura e o resultado opcional é nulo.
        let handle = unsafe {
            (api.create_file)(
                input.as_ptr(),
                GENERIC_READ,
                OPEN_EXISTING,
                0,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() {
            return Err(format!(
                "não foi possível abrir install.wim com a API do Windows: {}",
                std::io::Error::last_os_error()
            ));
        }
        let handle = WimHandle {
            raw: handle,
            close: api.close_handle,
        };
        let _callback = api.register(&handle, cancellation)?;
        cancellation.check()?;
        let mut part_size = i64::try_from(part_bytes)
            .map_err(|_| "o tamanho solicitado para o SWM é inválido".to_owned())?;
        // SAFETY: o handle e os ponteiros permanecem válidos durante a chamada;
        // WIMSplitFile recebe LARGE_INTEGER por ponteiro e flags reservados zero.
        let success = unsafe { (api.split_file)(handle.raw, output.as_ptr(), &mut part_size, 0) };
        cancellation.check()?;
        if success == 0 {
            return Err(format!(
                "não foi possível dividir install.wim com a API do Windows: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
fn split(input: &Path, output: &Path, bytes: u64) -> Result<(), String> {
    split_cancellable(input, output, bytes, &CancellationToken::default())
}
#[cfg(test)]
fn export_esd(input: &Path, output: &Path) -> Result<(), String> {
    export_esd_cancellable(input, output, &CancellationToken::default())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;

    fn c_path(path: &Path) -> Vec<u8> {
        let mut value = path.as_os_str().as_bytes().to_vec();
        value.push(0);
        value
    }

    fn assert_wim_status(status: i32) {
        assert_eq!(status, 0, "wimlib: {}", error_message(status));
    }

    #[test]
    fn bundled_wimlib_is_linked() {
        // SAFETY: esta função apenas retorna a versão numérica da biblioteca.
        let version = unsafe { wim_sys::wimlib_get_version() };
        assert_ne!(version, 0);
    }

    #[test]
    fn creates_and_splits_a_real_wim() {
        let temporary = tempfile::tempdir().unwrap();
        let source_directory = temporary.path().join("payload");
        std::fs::create_dir(&source_directory).unwrap();
        for index in 0..3u8 {
            let path = source_directory.join(format!("part-{index}.bin"));
            let mut file = std::fs::File::create(path).unwrap();
            let data = (0..(512 * 1024))
                .map(|offset| index.wrapping_add((offset % 251) as u8))
                .collect::<Vec<_>>();
            file.write_all(&data).unwrap();
        }

        let input_wim = temporary.path().join("install.wim");
        let output_swm = temporary.path().join("install.swm");
        let source = c_path(&source_directory);
        let output = c_path(&input_wim);
        let image_name = b"Meraki test\0";

        // SAFETY: todos os caminhos e o nome são terminados em NUL; os guards
        // mantêm a ordem correta entre WIMStruct e o estado global da biblioteca.
        let lock = WIM_LOCK.lock().unwrap();
        unsafe {
            assert_wim_status(wim_sys::wimlib_global_init(0));
            let global = WimGlobalGuard;
            let mut image = std::ptr::null_mut();
            assert_wim_status(wim_sys::wimlib_create_new_wim(
                wim_sys::wimlib_compression_type_WIMLIB_COMPRESSION_TYPE_NONE,
                &mut image,
            ));
            let image = WimImageGuard(image);
            assert_wim_status(wim_sys::wimlib_add_image(
                image.0,
                source.as_ptr().cast(),
                image_name.as_ptr().cast(),
                std::ptr::null(),
                0,
            ));
            assert_wim_status(wim_sys::wimlib_write(
                image.0,
                output.as_ptr().cast(),
                wim_sys::WIMLIB_ALL_IMAGES,
                0,
                1,
            ));
            drop(image);
            drop(global);
        }
        drop(lock);

        split(&input_wim, &output_swm, 700_000).unwrap();

        assert!(output_swm.is_file());
        assert!(temporary.path().join("install2.swm").is_file());
        let parts = std::fs::read_dir(temporary.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("swm"))
            })
            .count();
        assert!(parts >= 2);
    }
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    const SOLID_ESD: &[u8] = include_bytes!("../../tests/fixtures/install-solid.esd");

    #[test]
    fn exports_all_indices_from_a_real_solid_esd_and_removes_intermediate() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("install.esd");
        let output = temporary.path().join("install.swm");
        std::fs::write(&input, SOLID_ESD).unwrap();
        split_or_export_image(&input, &output, true, 3_000_000).unwrap();

        let parts = std::fs::read_dir(temporary.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "swm"))
            .collect::<Vec<_>>();
        assert!(parts.len() >= 3);
        for part in parts {
            let bytes = std::fs::read(part).unwrap();
            assert!(bytes.len() <= 3_000_000);
            assert_eq!(&bytes[..8], b"MSWIM\0\0\0");
        }
        let header = std::fs::read(&output).unwrap();
        // image_count do cabeçalho WIM: ambos os índices devem ser preservados.
        assert_eq!(u32::from_le_bytes(header[44..48].try_into().unwrap()), 2);
        assert_eq!(std::fs::read(&input).unwrap(), SOLID_ESD);
        assert!(std::fs::read_dir(temporary.path()).unwrap().all(|entry| {
            let path = entry.unwrap().path();
            path == input || path.extension().is_some_and(|extension| extension == "swm")
        }));
    }

    #[test]
    fn removes_conversion_workspace_when_the_esd_is_invalid() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("invalid.esd");
        std::fs::write(&input, b"not an image").unwrap();
        assert!(
            split_or_export_image(
                &input,
                &temporary.path().join("install.swm"),
                true,
                3_000_000
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read(input).unwrap(), b"not an image");
    }

    #[test]
    fn removes_intermediate_when_splitting_fails_after_export() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("install.esd");
        let output = temporary.path().join("install.swm");
        std::fs::write(&input, SOLID_ESD).unwrap();
        // Um diretório no lugar da saída força erro depois da exportação.
        std::fs::create_dir(&output).unwrap();
        assert!(split_or_export_image(&input, &output, true, 3_000_000).is_err());
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 2);
        assert_eq!(std::fs::read(input).unwrap(), SOLID_ESD);
    }

    #[test]
    fn continues_to_split_non_solid_wim_directly() {
        let temporary = tempfile::tempdir().unwrap();
        let esd = temporary.path().join("input.esd");
        let wim = temporary.path().join("install.wim");
        let swm = temporary.path().join("install.swm");
        std::fs::write(&esd, SOLID_ESD).unwrap();
        export_esd(&esd, &wim).unwrap();
        split_or_export_image(&wim, &swm, false, 3_000_000).unwrap();
        assert!(wim.is_file());
        assert!(swm.is_file());
        assert!(temporary.path().join("install2.swm").is_file());
    }

    #[test]
    fn rejects_invalid_part_sizes_before_creating_files() {
        let temporary = tempfile::tempdir().unwrap();
        for size in [0, u64::MAX] {
            assert!(
                split_or_export_image(
                    &temporary.path().join("input.esd"),
                    &temporary.path().join("install.swm"),
                    true,
                    size
                )
                .is_err()
            );
        }
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 0);
    }
}
