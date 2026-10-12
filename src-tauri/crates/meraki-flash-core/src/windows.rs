use crate::{DiscoveryError, UsbDevice, stable_device_id};
use serde::Deserialize;
use std::os::windows::process::CommandExt;
use std::process::Command;

const POWERSHELL: &str = "powershell.exe";
const DISCOVERY_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$result = @(
  foreach ($disk in Get-CimInstance Win32_DiskDrive) {
    $diskInfo = Get-Disk -Number $disk.Index -ErrorAction SilentlyContinue
    # Sem metadados de segurança, não é possível descartar discos de boot/sistema.
    if (-not $diskInfo -or $null -eq $diskInfo.IsSystem -or $null -eq $diskInfo.IsBoot) { continue }
    if ($disk.InterfaceType -ne 'USB' -and $diskInfo.BusType -ne 'USB') { continue }
    if ($diskInfo.IsSystem -eq $true -or $diskInfo.IsBoot -eq $true) { continue }
    if ([uint64]$disk.Size -eq 0) { continue }
    $logical = @(
      Get-CimAssociatedInstance -InputObject $disk -Association Win32_DiskDriveToDiskPartition |
        ForEach-Object { Get-CimAssociatedInstance -InputObject $_ -Association Win32_LogicalDiskToPartition }
    )
    $mounts = @($logical | ForEach-Object { $_.DeviceID + '\' })
    if ($mounts -contains ($env:SystemDrive + '\')) { continue }
    [PSCustomObject]@{
      DevicePath = [string]$disk.DeviceID
      Name = [string]$disk.Model
      Serial = [string]$disk.SerialNumber
      Size = [uint64]$disk.Size
      Kind = if ($disk.MediaType -match 'Removable' -or $diskInfo.IsRemovable -eq $true) { 'Removable' } else { 'Fixed USB' }
      ReadOnly = [bool]($diskInfo -and $diskInfo.IsReadOnly)
      MountPoints = $mounts
      FileSystem = [string](($logical | Select-Object -First 1).FileSystem)
      AvailableBytes = [uint64](($logical | Measure-Object -Property FreeSpace -Sum).Sum)
      IsSystem = [bool](($mounts -contains ($env:SystemDrive + '\')) -or ($diskInfo -and ($diskInfo.IsSystem -or $diskInfo.IsBoot)))
    }
  }
)
ConvertTo-Json -InputObject $result -Compress -Depth 5
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsDisk {
    device_path: String,
    name: String,
    serial: String,
    size: u64,
    kind: String,
    read_only: bool,
    #[serde(default)]
    mount_points: Vec<String>,
    file_system: String,
    available_bytes: u64,
    is_system: bool,
}

pub(super) fn discover_removable_devices() -> Result<Vec<UsbDevice>, DiscoveryError> {
    let output = Command::new(POWERSHELL)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            DISCOVERY_SCRIPT,
        ])
        .output()
        .map_err(|source| DiscoveryError::Command {
            program: POWERSHELL,
            source,
        })?;

    if !output.status.success() {
        return Err(DiscoveryError::CommandFailed {
            program: POWERSHELL,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let parsed: Vec<WindowsDisk> =
        serde_json::from_slice(&output.stdout).map_err(|source| DiscoveryError::InvalidOutput {
            program: POWERSHELL,
            source,
        })?;
    Ok(devices_from_windows_disks(parsed))
}

fn devices_from_windows_disks(parsed: Vec<WindowsDisk>) -> Vec<UsbDevice> {
    let mut devices = parsed
        .into_iter()
        .filter(|disk| !disk.is_system && disk.size > 0)
        .map(|disk| {
            let serial = clean_value(disk.serial);
            let first_mount = disk.mount_points.first().cloned().unwrap_or_default();
            UsbDevice {
                id: stable_device_id(&disk.device_path, serial.as_deref(), disk.size),
                name: clean_value(disk.name).unwrap_or_else(|| "Unidade USB".to_owned()),
                device_path: disk.device_path,
                mount_point: first_mount,
                mount_points: disk.mount_points,
                file_system: disk.file_system.trim().to_uppercase(),
                total_bytes: disk.size,
                available_bytes: disk.available_bytes,
                read_only: disk.read_only,
                kind: disk.kind,
                transport: "USB".to_owned(),
                serial,
            }
        })
        .collect::<Vec<_>>();

    devices.sort_by_key(|device| device.name.to_lowercase());
    devices
}

fn clean_value(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn disk_fixture() -> Value {
        json!({
            "Index": 1, "DeviceID": "\\\\.\\PhysicalDrive1", "Model": " USB ",
            "SerialNumber": " ABC ", "Size": 16000000000u64,
            "InterfaceType": "USB", "MediaType": "Fixed hard disk media",
            "Info": { "BusType": "USB", "IsSystem": false, "IsBoot": false,
                      "IsRemovable": false, "IsReadOnly": false },
            "Logical": [{ "DeviceID": "E:", "FileSystem": "exfat", "FreeSpace": 1024 }]
        })
    }

    // Executa o script de produção com cmdlets simulados, sem consultar discos reais.
    fn discover_fixture(disk: Value) -> Vec<UsbDevice> {
        let fixture = serde_json::to_string(&disk).unwrap().replace('\'', "''");
        let script = format!(
            r#"
$fixture = ConvertFrom-Json '{fixture}'
$env:SystemDrive = 'C:'
function Get-CimInstance {{ $fixture }}
function Get-Disk {{ param($Number, $ErrorAction) $fixture.Info }}
function Get-CimAssociatedInstance {{
    param($InputObject, $Association)
    if ($Association -eq 'Win32_DiskDriveToDiskPartition') {{ $InputObject }}
    else {{ $InputObject.Logical }}
}}
{DISCOVERY_SCRIPT}
"#
        );
        let output = Command::new(POWERSHELL)
            .creation_flags(0x08000000)
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let disks = serde_json::from_slice(&output.stdout).unwrap();
        devices_from_windows_disks(disks)
    }

    #[test]
    fn accepts_fixed_usb_with_either_bus_indicator_and_preserves_metadata() {
        for (interface, bus) in [("USB", "USB"), ("USB", "SCSI"), ("SCSI", "USB")] {
            for media in [
                json!("Fixed hard disk media"),
                json!("External hard disk media"),
                Value::Null,
            ] {
                let mut disk = disk_fixture();
                disk["InterfaceType"] = json!(interface);
                disk["Info"]["BusType"] = json!(bus);
                disk["Info"]["IsReadOnly"] = json!(true);
                disk["MediaType"] = media;
                let devices = discover_fixture(disk);
                assert_eq!(devices.len(), 1);
                let device = &devices[0];
                assert_eq!(device.kind, "Fixed USB");
                assert_eq!(device.transport, "USB");
                assert_eq!(device.name, "USB");
                assert_eq!(device.serial.as_deref(), Some("ABC"));
                assert_eq!(device.mount_points, ["E:\\"]);
                assert_eq!(device.file_system, "EXFAT");
                assert_eq!(device.available_bytes, 1024);
                assert!(device.read_only);
            }
        }
    }

    #[test]
    fn accepts_unmounted_fixed_usb() {
        let mut disk = disk_fixture();
        disk["Logical"] = json!([]);
        let devices = discover_fixture(disk);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].kind, "Fixed USB");
        assert!(devices[0].mount_points.is_empty());
        assert!(devices[0].mount_point.is_empty());
        assert_eq!(devices[0].available_bytes, 0);
    }

    #[test]
    fn classifies_removable_media_from_either_indicator() {
        for by_media in [false, true] {
            let mut disk = disk_fixture();
            if by_media {
                disk["MediaType"] = json!("Removable Media");
            } else {
                disk["Info"]["IsRemovable"] = json!(true);
            }
            let devices = discover_fixture(disk);
            assert_eq!(devices.len(), 1);
            assert_eq!(devices[0].kind, "Removable");
        }
    }

    #[test]
    fn rejects_internal_system_boot_empty_and_unverifiable_disks() {
        for case in [
            "sata",
            "nvme",
            "system",
            "boot",
            "system_mount",
            "empty",
            "no_info",
            "unknown_system",
            "unknown_boot",
        ] {
            let mut disk = disk_fixture();
            match case {
                "sata" | "nvme" => {
                    disk["InterfaceType"] = json!("SCSI");
                    disk["Info"]["BusType"] = json!(case);
                }
                "system" => disk["Info"]["IsSystem"] = json!(true),
                "boot" => disk["Info"]["IsBoot"] = json!(true),
                "system_mount" => disk["Logical"][0]["DeviceID"] = json!("c:"),
                "empty" => disk["Size"] = json!(0),
                "no_info" => disk["Info"] = Value::Null,
                "unknown_system" => {
                    disk["Info"].as_object_mut().unwrap().remove("IsSystem");
                }
                "unknown_boot" => {
                    disk["Info"].as_object_mut().unwrap().remove("IsBoot");
                }
                _ => unreachable!(),
            }
            assert!(discover_fixture(disk).is_empty(), "{case}");
        }
    }
}
