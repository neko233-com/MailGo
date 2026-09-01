#[cfg(target_os = "windows")]
mod windows_instance {
    use std::ptr::null;

    use anyhow::{anyhow, Result};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    const MUTEX_NAME: &[u16] = &[
        76, 111, 99, 97, 108, 92, 77, 97, 105, 108, 71, 111, 46, 83, 105, 110, 103, 108, 101, 73,
        110, 115, 116, 97, 110, 99, 101, 0,
    ];

    pub struct Guard(isize);

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0 as _);
            }
        }
    }

    pub fn acquire() -> Result<Option<Guard>> {
        acquire_named(MUTEX_NAME)
    }

    fn acquire_named(name: &[u16]) -> Result<Option<Guard>> {
        if name.last() != Some(&0) {
            return Err(anyhow!("single-instance mutex name is not terminated"));
        }
        unsafe {
            let handle = CreateMutexW(null(), 1, name.as_ptr());
            if handle == 0 {
                return Err(anyhow!("could not create MailGo single-instance mutex"));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return Ok(None);
            }
            Ok(Some(Guard(handle as isize)))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn named_mutex_rejects_a_second_instance_and_releases_on_drop() {
            let name = format!("Local\\MailGo.SingleInstance.Test.{}\0", std::process::id())
                .encode_utf16()
                .collect::<Vec<_>>();
            let first = acquire_named(&name)
                .expect("acquire first fixture mutex")
                .expect("first fixture instance");
            assert!(acquire_named(&name)
                .expect("probe second fixture mutex")
                .is_none());
            drop(first);
            assert!(acquire_named(&name)
                .expect("reacquire released fixture mutex")
                .is_some());
        }

        #[test]
        fn named_mutex_requires_a_terminated_windows_name() {
            assert!(acquire_named(&[77, 97, 105, 108, 71, 111]).is_err());
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_instance::acquire;

#[cfg(not(target_os = "windows"))]
pub struct Guard;

#[cfg(not(target_os = "windows"))]
pub fn acquire() -> anyhow::Result<Option<Guard>> {
    Ok(Some(Guard))
}
