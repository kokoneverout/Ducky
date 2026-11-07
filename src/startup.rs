//! Per-user logon startup. The registry entry is the source of truth, so an
//! unsuccessful write can never leave a persisted checkbox claiming success.
use anyhow::{Context, Result, ensure};
use std::{
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR},
    System::Registry::*,
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "ASRInput";

pub fn enabled() -> Result<bool> {
    Ok(read_value(RUN, VALUE)?.is_some_and(|value| !value.is_empty()))
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let command = if enabled {
        Some(command(
            &std::env::current_exe().context("无法获取程序路径")?,
        )?)
    } else {
        None
    };
    write_value(RUN, VALUE, command.as_deref())
}

fn command(exe: &Path) -> Result<Vec<u16>> {
    ensure!(exe.is_absolute(), "启动程序必须使用绝对路径");
    let path = exe.as_os_str().encode_wide().collect::<Vec<_>>();
    ensure!(
        !path.iter().any(|c| matches!(*c, 0 | 34)),
        "启动程序路径无效"
    );
    // Quote the executable independently of the argument, including Unicode and
    // spaces. Windows Run commands have a documented 260-character limit.
    let mut value = vec![b'"' as u16];
    value.extend(path);
    value.extend("\" --background".encode_utf16());
    ensure!(
        value.len() <= 260,
        "程序路径过长，无法设置开机启动，请移到较短路径后重试"
    );
    value.push(0);
    Ok(value)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn check(code: WIN32_ERROR, action: &str) -> Result<()> {
    if code == ERROR_SUCCESS {
        return Ok(());
    }
    Err(std::io::Error::from_raw_os_error(code as i32)).with_context(|| action.to_string())
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns one handle returned by Open/CreateKey.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn read_value(path: &str, name: &str) -> Result<Option<Vec<u16>>> {
    let mut buffer = [0u16; 1024];
    let mut size = size_of_val(&buffer) as u32;
    // SAFETY: strings are terminated, the output buffer is sized in bytes and
    // RegGetValue validates REG_SZ and terminates strings before returning.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(code, "读取开机启动设置失败")?;
    let length = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    Ok(Some(buffer[..length].to_vec()))
}

fn write_value(path: &str, name: &str, value: Option<&[u16]>) -> Result<()> {
    let mut handle = null_mut();
    let path = wide(path);
    // SAFETY: only the current user's named key is opened, with value-write
    // rights; no administrator or machine-wide registry access is requested.
    let code = unsafe {
        if value.is_some() {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                null(),
                &mut handle,
                null_mut(),
            )
        } else {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut handle,
            )
        }
    };
    if value.is_none() && code == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(code, "打开开机启动设置失败")?;
    let key = Key(handle);
    // SAFETY: value is a terminated UTF-16 command, or only this app's exact
    // value is removed. Other applications' startup entries are never changed.
    let code = unsafe {
        if let Some(value) = value {
            RegSetValueExW(
                key.0,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                size_of_val(value) as u32,
            )
        } else {
            RegDeleteValueW(key.0, wide(name).as_ptr())
        }
    };
    if value.is_none() && code == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(code, "修改开机启动设置失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_unicode_and_spaces_and_adds_background_flag() {
        let value = command(Path::new(r"D:\应用 程序\ASRInput\asr-input.exe")).unwrap();
        assert_eq!(
            String::from_utf16(&value[..value.len() - 1]).unwrap(),
            "\"D:\\应用 程序\\ASRInput\\asr-input.exe\" --background"
        );
        assert_eq!(value.last(), Some(&0));
    }

    #[test]
    fn invalid_and_overlong_commands_are_rejected() {
        assert!(command(Path::new("asr-input.exe")).is_err());
        assert!(command(Path::new("D:\\bad\"path\\asr-input.exe")).is_err());
        assert!(
            command(Path::new(&format!(
                "D:\\{}\\asr-input.exe",
                "a".repeat(260)
            )))
            .is_err()
        );
    }

    #[test]
    fn registry_round_trip_and_disable_only_touch_the_named_value() {
        // Never test by registering a real startup item. Use a fresh, private
        // test key, and remove only that non-recursive key on every exit path.
        struct TestKey(String);
        impl Drop for TestKey {
            fn drop(&mut self) {
                unsafe {
                    RegDeleteKeyW(HKEY_CURRENT_USER, wide(&self.0).as_ptr());
                }
            }
        }
        let key = TestKey(format!(
            r"Software\ASRInput.StartupTest-{}",
            std::process::id()
        ));
        let value = command(Path::new(r"D:\测试 目录\asr-input.exe")).unwrap();
        assert!(read_value(&key.0, VALUE).unwrap().is_none());
        write_value(&key.0, "Unrelated", Some(&wide("leave intact"))).unwrap();
        write_value(&key.0, VALUE, Some(&value)).unwrap();
        assert_eq!(
            read_value(&key.0, VALUE).unwrap().unwrap(),
            value[..value.len() - 1]
        );
        write_value(&key.0, VALUE, None).unwrap();
        write_value(&key.0, VALUE, None).unwrap();
        assert!(read_value(&key.0, VALUE).unwrap().is_none());
        assert_eq!(
            read_value(&key.0, "Unrelated").unwrap().unwrap(),
            wide("leave intact")[..12]
        );
    }
}
