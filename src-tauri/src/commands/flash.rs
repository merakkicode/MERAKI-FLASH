use crate::flash::protocol::{StartFlashRequest, StartFlashResponse, StartFormatRequest};
use crate::flash::{self, FlashManager};
use tauri::{AppHandle, State};

#[tauri::command]
pub async fn cancel_flash(manager: State<'_, FlashManager>) -> Result<(), String> {
    manager.cancel()
}

#[tauri::command]
pub async fn start_format_device(
    app: AppHandle,
    manager: State<'_, FlashManager>,
    request: StartFormatRequest,
) -> Result<StartFlashResponse, String> {
    flash::start_format_device(app, &manager, request).await
}

#[tauri::command]
pub async fn start_flash(
    app: AppHandle,
    manager: State<'_, FlashManager>,
    request: StartFlashRequest,
) -> Result<StartFlashResponse, String> {
    flash::start_flash(app, &manager, request).await
}
