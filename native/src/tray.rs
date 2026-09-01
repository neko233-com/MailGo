#[cfg(target_os = "windows")]
mod windows_tray {
    use std::mem::{size_of, zeroed};
    use std::path::PathBuf;
    use std::ptr::null;
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD,
        NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CallWindowProcW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, PeekMessageW,
        PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
        SetWindowLongPtrW, ShowWindow, TrackPopupMenu, TranslateMessage, GWLP_WNDPROC,
        HWND_MESSAGE, IMAGE_ICON, LR_LOADFROMFILE, MF_STRING, PM_REMOVE, SW_HIDE, SW_RESTORE,
        SW_SHOW, TPM_RIGHTALIGN, WM_APP, WM_CLOSE, WM_COMMAND, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
        WM_RBUTTONUP, WNDCLASSW, WNDPROC,
    };

    const TRAY_CALLBACK: u32 = WM_APP + 73;
    const TRAY_ID: u32 = 1;
    const SHOW_COMMAND: usize = 1001;
    const QUIT_COMMAND: usize = 1002;
    const WINDOW_TITLE: &[u16] = &[77, 97, 105, 108, 71, 111, 0];
    const TRAY_CLASS: &[u16] = &[77, 97, 105, 108, 71, 111, 84, 114, 97, 121, 0];

    static TARGET_WINDOW: AtomicIsize = AtomicIsize::new(0);
    static TRAY_WINDOW: AtomicIsize = AtomicIsize::new(0);
    static TRAY_ICON: AtomicIsize = AtomicIsize::new(0);
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
    static PREVIOUS_WNDPROC: AtomicIsize = AtomicIsize::new(0);
    static MINIMIZE_TO_TRAY: AtomicBool = AtomicBool::new(true);
    static ALLOW_CLOSE: AtomicBool = AtomicBool::new(false);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrayAction {
        RestoreIcon,
        ShowWindow,
        ShowMenu,
        Quit,
        Ignore,
        Default,
    }

    pub fn start(minimize_to_tray: bool) {
        MINIMIZE_TO_TRAY.store(minimize_to_tray, Ordering::Relaxed);
        thread::Builder::new()
            .name("mailgo-tray".into())
            .spawn(|| unsafe { run() })
            .expect("start MailGo tray thread");
    }

    pub fn set_minimize_to_tray(enabled: bool) {
        MINIMIZE_TO_TRAY.store(enabled, Ordering::Relaxed);
    }

    pub fn hide_main_window() {
        unsafe {
            let hwnd = main_window();
            if hwnd != 0 {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    pub fn activate_main_window() {
        unsafe { show_main_window() }
    }

    unsafe fn run() {
        let instance = GetModuleHandleW(null());
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(tray_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: 0,
            hCursor: 0,
            hbrBackground: 0,
            lpszMenuName: null(),
            lpszClassName: TRAY_CLASS.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            tracing::warn!("MailGo tray window class registration failed");
        }
        let taskbar_created_name = "TaskbarCreated\0".encode_utf16().collect::<Vec<_>>();
        let taskbar_created = RegisterWindowMessageW(taskbar_created_name.as_ptr());
        if taskbar_created == 0 {
            tracing::warn!("MailGo could not register the TaskbarCreated notification");
        }
        TASKBAR_CREATED.store(taskbar_created, Ordering::Release);

        let tray_window = CreateWindowExW(
            0,
            TRAY_CLASS.as_ptr(),
            WINDOW_TITLE.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            0,
            instance,
            null(),
        );
        if tray_window == 0 {
            tracing::warn!("MailGo tray window creation failed");
            return;
        }
        TRAY_WINDOW.store(tray_window, Ordering::Release);

        let icon = load_icon();
        TRAY_ICON.store(icon as isize, Ordering::Release);
        let notification = tray_notification(tray_window);
        if Shell_NotifyIconW(NIM_ADD, &notification) == 0 {
            tracing::warn!("MailGo tray icon could not be registered");
        }

        attach_main_window();

        let mut message: windows_sys::Win32::UI::WindowsAndMessaging::MSG = zeroed();
        loop {
            while PeekMessageW(&mut message, 0, 0, 0, PM_REMOVE) != 0 {
                if message.message == windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT {
                    Shell_NotifyIconW(NIM_DELETE, &notification);
                    TRAY_WINDOW.store(0, Ordering::Release);
                    TRAY_ICON.store(0, Ordering::Release);
                    TASKBAR_CREATED.store(0, Ordering::Release);
                    DestroyWindow(tray_window);
                    return;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            if TARGET_WINDOW.load(Ordering::Relaxed) == 0 {
                attach_main_window();
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    unsafe fn attach_main_window() {
        let hwnd = FindWindowW(null(), WINDOW_TITLE.as_ptr());
        if hwnd == 0 {
            thread::sleep(Duration::from_millis(250));
            return;
        }

        let mut process_id = 0u32;
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            hwnd,
            &mut process_id,
        );
        if process_id != GetCurrentProcessId() {
            return;
        }
        if TARGET_WINDOW
            .compare_exchange(0, hwnd, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            let previous =
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, main_window_proc as *const () as isize);
            PREVIOUS_WNDPROC.store(previous, Ordering::SeqCst);
            tracing::info!("MailGo tray lifecycle attached to the main window");
        }
    }

    unsafe fn tray_notification(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut notification: NOTIFYICONDATAW = zeroed();
        notification.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        notification.hWnd = hwnd;
        notification.uID = TRAY_ID;
        notification.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        notification.uCallbackMessage = TRAY_CALLBACK;
        notification.hIcon = TRAY_ICON.load(Ordering::Acquire) as _;
        copy_tip(&mut notification.szTip, "MailGo");
        notification
    }

    unsafe fn load_icon() -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
        let mut candidates = Vec::<PathBuf>::new();
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                candidates.push(parent.join("mailgo.ico"));
                candidates.push(parent.join("resources/icons/mailgo.ico"));
                candidates.push(parent.join("../resources/icons/mailgo.ico"));
            }
        }
        if let Ok(current) = std::env::current_dir() {
            candidates.push(current.join("resources/icons/mailgo.ico"));
        }
        candidates.push(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../resources/icons/mailgo.ico"),
        );
        for candidate in candidates {
            if !candidate.is_file() {
                continue;
            }
            let path = candidate
                .to_string_lossy()
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<_>>();
            let icon = windows_sys::Win32::UI::WindowsAndMessaging::LoadImageW(
                0,
                path.as_ptr(),
                IMAGE_ICON,
                32,
                32,
                LR_LOADFROMFILE,
            ) as _;
            if icon != 0 {
                return icon;
            }
        }
        windows_sys::Win32::UI::WindowsAndMessaging::LoadIconW(
            0,
            windows_sys::Win32::UI::WindowsAndMessaging::IDI_APPLICATION,
        )
    }

    fn copy_tip(target: &mut [u16; 128], value: &str) {
        let limit = target.len().saturating_sub(1);
        for (slot, character) in target.iter_mut().take(limit).zip(value.encode_utf16()) {
            *slot = character;
        }
    }

    pub fn notify_new_mail(title: &str, message: &str) {
        let hwnd = TRAY_WINDOW.load(Ordering::Acquire) as HWND;
        if hwnd == 0 {
            return;
        }
        unsafe {
            let mut notification: NOTIFYICONDATAW = zeroed();
            notification.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            notification.hWnd = hwnd;
            notification.uID = TRAY_ID;
            notification.uFlags = NIF_INFO;
            copy_text(&mut notification.szInfoTitle, title);
            copy_text(&mut notification.szInfo, message);
            notification.dwInfoFlags = NIIF_INFO;
            if Shell_NotifyIconW(NIM_MODIFY, &notification) == 0 {
                let restored = tray_notification(hwnd);
                Shell_NotifyIconW(NIM_ADD, &restored);
            }
        }
    }

    fn copy_text<const N: usize>(target: &mut [u16; N], value: &str) {
        for (slot, character) in target
            .iter_mut()
            .take(N.saturating_sub(1))
            .zip(value.encode_utf16())
        {
            *slot = character;
        }
    }

    unsafe extern "system" fn main_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_CLOSE
            && MINIMIZE_TO_TRAY.load(Ordering::Relaxed)
            && !ALLOW_CLOSE.swap(false, Ordering::Relaxed)
        {
            ShowWindow(hwnd, SW_HIDE);
            return 0;
        }

        let previous = PREVIOUS_WNDPROC.load(Ordering::Relaxed);
        if previous != 0 {
            let previous: WNDPROC = std::mem::transmute(previous);
            return CallWindowProcW(previous, hwnd, message, wparam, lparam);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    unsafe extern "system" fn tray_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let taskbar_created = TASKBAR_CREATED.load(Ordering::Acquire);
        match tray_action(message, wparam, lparam, taskbar_created) {
            TrayAction::RestoreIcon => {
                let notification = tray_notification(hwnd);
                if Shell_NotifyIconW(NIM_ADD, &notification) == 0 {
                    tracing::warn!("MailGo tray icon could not be restored after taskbar restart");
                }
                0
            }
            TrayAction::ShowWindow => {
                show_main_window();
                0
            }
            TrayAction::ShowMenu => {
                show_menu(hwnd);
                0
            }
            TrayAction::Quit => {
                ALLOW_CLOSE.store(true, Ordering::Relaxed);
                let target = TARGET_WINDOW.load(Ordering::Relaxed) as HWND;
                if target != 0 {
                    PostMessageW(target, WM_CLOSE, 0, 0);
                }
                PostQuitMessage(0);
                0
            }
            TrayAction::Ignore => 0,
            TrayAction::Default => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    fn tray_action(
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        taskbar_created: u32,
    ) -> TrayAction {
        if taskbar_created != 0 && message == taskbar_created {
            return TrayAction::RestoreIcon;
        }
        if message == TRAY_CALLBACK {
            return match lparam as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => TrayAction::ShowWindow,
                WM_RBUTTONUP => TrayAction::ShowMenu,
                _ => TrayAction::Ignore,
            };
        }
        if message == WM_COMMAND {
            return match wparam & 0xffff {
                SHOW_COMMAND => TrayAction::ShowWindow,
                QUIT_COMMAND => TrayAction::Quit,
                _ => TrayAction::Ignore,
            };
        }
        TrayAction::Default
    }

    unsafe fn show_main_window() {
        let hwnd = main_window();
        if hwnd == 0 {
            return;
        }
        ShowWindow(hwnd, SW_SHOW);
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
    }

    unsafe fn main_window() -> HWND {
        let attached = TARGET_WINDOW.load(Ordering::Relaxed) as HWND;
        if attached != 0 {
            attached
        } else {
            FindWindowW(null(), WINDOW_TITLE.as_ptr())
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        let menu = CreatePopupMenu();
        if menu == 0 {
            return;
        }
        let show = "打开 MailGo\0".encode_utf16().collect::<Vec<_>>();
        let quit = "退出 MailGo\0".encode_utf16().collect::<Vec<_>>();
        AppendMenuW(menu, MF_STRING, SHOW_COMMAND, show.as_ptr());
        AppendMenuW(menu, MF_STRING, QUIT_COMMAND, quit.as_ptr());
        let mut point: POINT = zeroed();
        GetCursorPos(&mut point);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTALIGN, point.x, point.y, 0, hwnd, null());
        DestroyMenu(menu);
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::{Mutex, OnceLock};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            IsWindow, IsWindowVisible, WS_OVERLAPPEDWINDOW,
        };

        static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        const STATIC_CLASS: &[u16] = &[83, 84, 65, 84, 73, 67, 0];
        const TEST_TITLE: &[u16] = &[
            77, 97, 105, 108, 71, 111, 32, 84, 114, 97, 121, 32, 84, 101, 115, 116, 0,
        ];

        unsafe fn create_test_window() -> HWND {
            CreateWindowExW(
                0,
                STATIC_CLASS.as_ptr(),
                TEST_TITLE.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                -32_000,
                -32_000,
                320,
                240,
                0,
                0,
                GetModuleHandleW(null()),
                null(),
            )
        }

        #[test]
        fn tray_message_router_covers_restore_show_menu_and_quit() {
            let taskbar_created = WM_APP + 400;
            assert_eq!(
                tray_action(taskbar_created, 0, 0, taskbar_created),
                TrayAction::RestoreIcon
            );
            assert_eq!(
                tray_action(TRAY_CALLBACK, 0, WM_LBUTTONUP as LPARAM, 0),
                TrayAction::ShowWindow
            );
            assert_eq!(
                tray_action(TRAY_CALLBACK, 0, WM_LBUTTONDBLCLK as LPARAM, 0),
                TrayAction::ShowWindow
            );
            assert_eq!(
                tray_action(TRAY_CALLBACK, 0, WM_RBUTTONUP as LPARAM, 0),
                TrayAction::ShowMenu
            );
            assert_eq!(
                tray_action(WM_COMMAND, SHOW_COMMAND, 0, 0),
                TrayAction::ShowWindow
            );
            assert_eq!(
                tray_action(WM_COMMAND, QUIT_COMMAND, 0, 0),
                TrayAction::Quit
            );
            assert_eq!(tray_action(WM_COMMAND, 65_535, 0, 0), TrayAction::Ignore);
        }

        #[test]
        fn close_hides_without_destroying_then_restore_and_quit_work() {
            let _guard = TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .expect("lock tray fixture");
            unsafe {
                let hwnd = create_test_window();
                assert_ne!(hwnd, 0);
                TARGET_WINDOW.store(hwnd, Ordering::SeqCst);
                PREVIOUS_WNDPROC.store(0, Ordering::SeqCst);
                MINIMIZE_TO_TRAY.store(true, Ordering::SeqCst);
                ALLOW_CLOSE.store(false, Ordering::SeqCst);
                ShowWindow(hwnd, SW_SHOW);
                assert_ne!(IsWindowVisible(hwnd), 0);

                assert_eq!(main_window_proc(hwnd, WM_CLOSE, 0, 0), 0);
                assert_ne!(IsWindow(hwnd), 0, "close-to-tray destroyed the window");
                assert_eq!(IsWindowVisible(hwnd), 0, "close-to-tray left it visible");

                show_main_window();
                assert_ne!(
                    IsWindowVisible(hwnd),
                    0,
                    "tray restore did not show the window"
                );

                ALLOW_CLOSE.store(true, Ordering::SeqCst);
                assert_eq!(main_window_proc(hwnd, WM_CLOSE, 0, 0), 0);
                assert_eq!(
                    IsWindow(hwnd),
                    0,
                    "explicit tray quit did not destroy the window"
                );
                TARGET_WINDOW.store(0, Ordering::SeqCst);
                PREVIOUS_WNDPROC.store(0, Ordering::SeqCst);
                ALLOW_CLOSE.store(false, Ordering::SeqCst);
            }
        }

        #[test]
        fn disabling_close_to_tray_allows_the_normal_close_path() {
            let _guard = TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .expect("lock tray fixture");
            unsafe {
                let hwnd = create_test_window();
                assert_ne!(hwnd, 0);
                TARGET_WINDOW.store(hwnd, Ordering::SeqCst);
                PREVIOUS_WNDPROC.store(0, Ordering::SeqCst);
                MINIMIZE_TO_TRAY.store(false, Ordering::SeqCst);
                ALLOW_CLOSE.store(false, Ordering::SeqCst);
                ShowWindow(hwnd, SW_SHOW);

                assert_eq!(main_window_proc(hwnd, WM_CLOSE, 0, 0), 0);
                assert_eq!(IsWindow(hwnd), 0);
                TARGET_WINDOW.store(0, Ordering::SeqCst);
                MINIMIZE_TO_TRAY.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_tray::notify_new_mail;
#[cfg(target_os = "windows")]
pub use windows_tray::{activate_main_window, hide_main_window, set_minimize_to_tray, start};

#[cfg(not(target_os = "windows"))]
pub fn start(_minimize_to_tray: bool) {}

#[cfg(not(target_os = "windows"))]
pub fn set_minimize_to_tray(_enabled: bool) {}

#[cfg(not(target_os = "windows"))]
pub fn hide_main_window() {}

#[cfg(not(target_os = "windows"))]
pub fn activate_main_window() {}

#[cfg(not(target_os = "windows"))]
pub fn notify_new_mail(_title: &str, _message: &str) {}
