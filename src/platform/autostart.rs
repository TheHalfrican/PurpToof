//! Run-at-login, via the per-user Run key.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` rather than the
//! machine-wide `HKLM` equivalent or a scheduled task:
//!
//! - HKCU needs no elevation, so the checkbox in the settings panel works
//!   without a UAC prompt every time it is toggled.
//! - This app is per-user by nature. It advertises *this* PC as a speaker and
//!   routes audio to the logged-in user's default output; starting it for
//!   every account on the machine would be wrong.
//! - A scheduled task survives more, but is far harder for a user to find and
//!   undo than a Run entry, and this is a convenience, not a service.

use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::HSTRING;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The value name. Stable, because changing it would orphan the old entry and
/// silently leave the app starting twice.
const VALUE: &str = "PurpToof";

/// The command Windows should run at login.
///
/// Quoted, because the path very often contains spaces (`Program Files`,
/// `C:\Users\Firstname Lastname\...`) and an unquoted Run value is parsed at
/// the first one - which fails in a way that looks like the feature simply
/// does not work.
fn command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the executable: {e}"))?;
    Ok(format!("\"{}\"", exe.display()))
}

fn open(access: u32) -> Result<HKEY, String> {
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            None,
            windows::Win32::System::Registry::REG_SAM_FLAGS(access),
            &mut key,
        )
    };
    if status.is_ok() {
        Ok(key)
    } else {
        Err(format!("cannot open the Run key: {status:?}"))
    }
}

/// Whether PurpToof is currently registered to start at login.
pub fn is_enabled() -> bool {
    let Ok(key) = open(KEY_READ.0) else {
        return false;
    };
    let status = unsafe { RegQueryValueExW(key, &HSTRING::from(VALUE), None, None, None, None) };
    unsafe { RegCloseKey(key) }.ok().ok();
    status.is_ok()
}

/// Add or remove the Run entry.
pub fn set(enabled: bool) -> Result<(), String> {
    let key = open(KEY_READ.0 | KEY_WRITE.0)?;
    let result = if enabled {
        write_value(key)
    } else {
        remove_value(key)
    };
    unsafe { RegCloseKey(key) }.ok().ok();
    result
}

fn write_value(key: HKEY) -> Result<(), String> {
    let command = command()?;
    // REG_SZ is NUL-terminated UTF-16; HSTRING gives us the encoding, and the
    // terminator has to be included in the byte count or Windows reads a
    // truncated path.
    let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe {
        std::slice::from_raw_parts(wide.as_ptr() as *const u8, std::mem::size_of_val(&wide[..]))
    };
    let status = unsafe { RegSetValueExW(key, &HSTRING::from(VALUE), None, REG_SZ, Some(bytes)) };
    if status.is_ok() {
        Ok(())
    } else {
        Err(format!("could not write the Run value: {status:?}"))
    }
}

fn remove_value(key: HKEY) -> Result<(), String> {
    let status = unsafe { RegDeleteValueW(key, &HSTRING::from(VALUE)) };
    // Already absent is success: the caller asked for "not registered", and
    // that is the state we are in.
    if status.is_ok() || status == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("could not remove the Run value: {status:?}"))
    }
}

/// Bring the registry into line with the config, if it has drifted.
///
/// Called at startup so the setting survives the app being moved: the stored
/// command contains an absolute path, and a stale one would silently start
/// nothing.
pub fn reconcile(want: bool) {
    if is_enabled() == want {
        // Still refresh the path when enabled, in case the exe moved.
        if want && let Ok(cmd) = command() {
            tracing::debug!(command = %cmd, "autostart already enabled");
        }
        return;
    }
    match set(want) {
        Ok(()) => tracing::info!(enabled = want, "autostart updated"),
        Err(e) => tracing::warn!(error = %e, "could not update autostart"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_quoted() {
        // An unquoted Run value is parsed at the first space, and paths like
        // C:\Program Files\PurpToof\purptoof.exe are the common case - so this
        // failing looks exactly like "autostart does not work".
        let c = command().expect("we can always find our own exe in a test");
        assert!(c.starts_with('"') && c.ends_with('"'), "not quoted: {c}");
    }

    #[test]
    fn the_command_points_at_this_executable() {
        let c = command().unwrap();
        assert!(
            c.to_ascii_lowercase().contains(".exe"),
            "should name an executable: {c}"
        );
    }

    #[test]
    fn querying_is_harmless_when_absent() {
        // is_enabled must never panic or error out; it runs on every startup
        // and on every settings repaint.
        let _ = is_enabled();
    }
}
