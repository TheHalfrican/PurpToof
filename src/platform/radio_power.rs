//! Reading - and, on request, changing - whether Windows may power down the
//! Bluetooth adapter.
//!
//! The reasoning about what the value *means* lives in
//! [`crate::core::power`]; this module only fetches it and, when the user
//! explicitly asks, launches the change. Per CLAUDE.md's layering rule there
//! is no branch here that decides anything.
//!
//! # Read via registry, write via WMI
//!
//! An asymmetry worth stating, because it looks like an inconsistency:
//!
//! - **Reading** is a `REG_DWORD` at
//!   `HKLM\SYSTEM\CurrentControlSet\Enum\<devnode>\Device Parameters\WDF`,
//!   value `IdleInWorkingState`. Readable unelevated, and a dozen lines.
//! - **Writing** goes through the `MSPower_DeviceEnable` WMI class, which is
//!   what Device Manager's checkbox drives. Opening that registry key for
//!   write is refused unelevated (measured 2026-09-16); whether an
//!   administrator would be permitted has **not** been tested. The reason for
//!   preferring WMI is not the ACL either way - it is that storing the value
//!   and applying the policy are different things, and WMI is the interface
//!   the driver actually acts on.
//!
//! # Why the write is a separate elevated process
//!
//! The WMI write requires administrator rights; measured 2026-09-16, an
//! unelevated `Set-CimInstance` returns `Access denied`. PurpToof autostarts
//! with Windows and sits in the tray, and giving a background audio utility a
//! permanent elevated token to flip one checkbox is a bad trade. So the change
//! is shelled out to a one-shot elevated process, which puts a UAC prompt
//! between the user and the change.
//!
//! That is also what keeps this compatible with CLAUDE.md's "the app should
//! not silently rewrite this". Nothing here runs on its own: the caller is a
//! button the user pressed, having been told exactly which setting changes.

use anyhow::{Context, Result, anyhow};
use windows::Devices::Bluetooth::BluetoothAdapter;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::core::HSTRING;

use crate::core::power::{RadioPowerPolicy, classify, devnode_id_from_interface_id};

/// The default adapter's devnode instance id, e.g.
/// `USB\VID_8087&PID_0033\5&3b72e4cf&0&14`.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread.
pub unsafe fn adapter_devnode_id() -> Result<String> {
    let adapter = BluetoothAdapter::GetDefaultAsync()
        .context("BluetoothAdapter::GetDefaultAsync dispatch failed")?
        .join()
        .context("BluetoothAdapter::GetDefaultAsync did not complete")?;

    let id = adapter
        .DeviceId()
        .context("the default adapter reported no DeviceId")?
        .to_string();

    devnode_id_from_interface_id(&id)
        .ok_or_else(|| anyhow!("adapter DeviceId is not an interface id we recognise: {id}"))
}

/// The adapter's current idle policy.
///
/// Every failure path collapses to [`RadioPowerPolicy::Unknown`], which does
/// not warn. A missing key, a denied read and a machine whose driver stores
/// this somewhere else are indistinguishable from here, and none of them is
/// evidence that the radio may power down.
pub fn policy_for(devnode_id: &str) -> RadioPowerPolicy {
    classify(read_idle_in_working_state(devnode_id))
}

fn read_idle_in_working_state(devnode_id: &str) -> Option<u32> {
    let subkey = HSTRING::from(format!(
        r"SYSTEM\CurrentControlSet\Enum\{devnode_id}\Device Parameters\WDF"
    ));
    let value = HSTRING::from("IdleInWorkingState");

    let mut data: u32 = 0;
    let mut size = size_of::<u32>() as u32;

    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            &value,
            RRF_RT_REG_DWORD,
            None,
            Some(&raw mut data as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };

    (status == ERROR_SUCCESS).then_some(data)
}

/// The exact change [`request_hold_radio_on`] will make, for display.
///
/// The UI shows this verbatim before the user commits. "One click" must not
/// mean "one click and you find out afterwards", and a UAC prompt naming
/// PowerShell explains nothing on its own.
pub const FIX_DESCRIPTION: &str = concat!(
    "Unticks \"Allow the computer to turn off this device to save power\" ",
    "for the Bluetooth adapter. This is the same checkbox as Device Manager ",
    "> the adapter > Power Management. Windows will ask for administrator ",
    "permission. Nothing else is changed."
);

/// Ask Windows to stop powering the adapter down.
///
/// Returns as soon as the elevated process has been **launched**, not when the
/// change has landed - `ShellExecuteExW` returns once the user answers the UAC
/// prompt, and the write happens after. The caller must re-read
/// [`policy_for`] to find out whether it actually took, rather than assuming
/// success from an `Ok(())` here.
///
/// An `Err` means the prompt was declined or the process could not start. That
/// is an ordinary outcome, not a fault: the user is allowed to say no.
pub fn request_hold_radio_on(devnode_id: &str) -> Result<()> {
    // The instance name WMI uses is the devnode id plus an index suffix, so
    // this matches by prefix. Single quotes stop PowerShell expanding the `$`
    // and `&` that device ids are full of.
    let script = format!(
        "$d = Get-CimInstance MSPower_DeviceEnable -Namespace root\\wmi | \
         Where-Object {{ $_.InstanceName -like '{}*' }}; \
         if ($d) {{ Set-CimInstance -InputObject $d -Property @{{ Enable = $false }} }}",
        devnode_id.replace('\'', "''")
    );
    let parameters = format!("-NoProfile -WindowStyle Hidden -Command \"{script}\"");

    let verb = HSTRING::from("runas");
    let file = HSTRING::from("powershell.exe");
    let params = HSTRING::from(parameters);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        lpVerb: windows::core::PCWSTR(verb.as_ptr()),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        // SW_HIDE. The console window would flash otherwise, which looks
        // exactly like something going wrong.
        nShow: 0,
        ..Default::default()
    };

    unsafe { ShellExecuteExW(&mut info) }
        .context("the elevated helper did not start (the prompt may have been declined)")
}
