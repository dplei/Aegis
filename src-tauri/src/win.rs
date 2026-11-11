//! 薄薄一层 Win32：进程、窗口、显示器、磁盘、回收站。
use std::path::Path;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// (pid, exe 名)
pub fn processes() -> Vec<(u32, String)> {
    let mut out = vec![];
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            out.push((e.th32ProcessID, from_wide(&e.szExeFile)));
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

#[derive(Clone, Debug)]
pub struct Win {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub area: i64,
}

/// 能被 WGC 抓的顶层窗口：可见、无 owner、非工具窗、未被系统隐藏、有标题、有面积。
pub fn windows() -> Vec<Win> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<Win>);
        if !IsWindowVisible(h).as_bool() || GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
            return true.into();
        }
        if GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
        if cloaked != 0 {
            return true.into();
        }
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(h, &mut buf);
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
        if n > 0 && area > 0 {
            let mut pid = 0;
            GetWindowThreadProcessId(h, Some(&mut pid));
            out.push(Win { hwnd: h.0 as isize, pid, title: from_wide(&buf), area });
        }
        true.into()
    }
    let mut out: Vec<Win> = vec![];
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

/// 窗口所在显示器：(HMONITOR, 宽, 高)
pub fn monitor_of(hwnd: isize) -> (isize, u32, u32) {
    unsafe {
        let m = MonitorFromWindow(HWND(hwnd as _), MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(m, &mut mi);
        let r = mi.rcMonitor;
        (m.0 as isize, (r.right - r.left) as u32, (r.bottom - r.top) as u32)
    }
}

pub fn disk_free(dir: &Path) -> u64 {
    let w = wide(&dir.to_string_lossy());
    let mut free = 0u64;
    unsafe {
        let _ = GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None);
    }
    free
}

pub fn recycle(path: &Path) -> bool {
    let mut from = wide(&path.to_string_lossy());
    from.push(0); // 双 0 结尾
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    unsafe { SHFileOperationW(&mut op) == 0 }
}

/// 「启动」文件夹里的 Aegis.lnk：注册表 Run 被安全软件拦截时的开机自启兜底。
pub fn startup_lnk() -> std::path::PathBuf {
    let appdata = std::env::var_os("APPDATA").unwrap_or_default();
    Path::new(&appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup\Aegis.lnk")
}

/// HKCU Run 下有没有这个值（不管任务管理器里是否禁用）
pub fn run_value_exists(name: &str) -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_ANY};
    let name = wide(name);
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Run"), PCWSTR(name.as_ptr()), RRF_RT_ANY, None, None, None).is_ok()
    }
}

/// on = 建（或覆盖）指向当前 exe 的快捷方式，带 args；off = 删掉。
pub fn set_startup_lnk(on: bool, args: &str) -> Result<(), String> {
    let lnk = startup_lnk();
    if !on {
        return match std::fs::remove_file(&lnk) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    use windows::core::Interface;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let (exe_w, args_w, lnk_w) = (wide(&exe.to_string_lossy()), wide(args), wide(&lnk.to_string_lossy()));
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); // 已初始化过（含不同模式）都无妨
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        link.SetPath(PCWSTR(exe_w.as_ptr())).map_err(|e| e.to_string())?;
        link.SetArguments(PCWSTR(args_w.as_ptr())).map_err(|e| e.to_string())?;
        let file: IPersistFile = link.cast().map_err(|e| e.to_string())?;
        file.Save(PCWSTR(lnk_w.as_ptr()), true).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recycle_and_disk() {
        let p = std::env::temp_dir().join("aegis-recycle-test.txt");
        std::fs::write(&p, "x").unwrap();
        assert!(super::recycle(&p));
        assert!(!p.exists());
        assert!(super::disk_free(&std::env::temp_dir()) > 0);
        assert!(!super::processes().is_empty());
    }
}

use wasapi::{initialize_mta, DeviceEnumerator, Direction};

/// (设备 id, 名称)
pub fn list_mics() -> Vec<(String, String)> {
    let _ = initialize_mta();
    let mut out = vec![];
    if let Ok(col) = DeviceEnumerator::new().and_then(|e| e.get_device_collection(Direction::Capture)) {
        for i in 0..col.get_nbr_devices().unwrap_or(0) {
            if let Ok(d) = col.get_device_at_index(i) {
                if let (Ok(id), Ok(name)) = (d.get_id(), d.get_friendlyname()) {
                    out.push((id, name));
                }
            }
        }
    }
    out
}

/// 系统设置里的默认录音设备 id
pub fn default_mic() -> Option<String> {
    let _ = initialize_mta();
    DeviceEnumerator::new().and_then(|e| e.get_default_device(Direction::Capture)).and_then(|d| d.get_id()).ok()
}

/// OBS 的窗口标识 "标题:类名:exe"（# 和 : 转义成 #22、#3A）。只读窗口本身和进程快照，不打开目标进程。
pub fn obs_window_id(hwnd: isize) -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let h = HWND(hwnd as _);
    let (mut t, mut c) = ([0u16; 256], [0u16; 256]); // 缓冲全 0，from_wide 读到第一个 0 为止
    unsafe {
        GetWindowTextW(h, &mut t);
        GetClassNameW(h, &mut c);
    }
    let (title, class) = (from_wide(&t), from_wide(&c));
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    let exe = processes().into_iter().find(|p| p.0 == pid)?.1;
    let enc = |s: &str| s.replace('#', "#22").replace(':', "#3A");
    Some(format!("{}:{}:{}", enc(&title), enc(&class), enc(&exe)))
}
