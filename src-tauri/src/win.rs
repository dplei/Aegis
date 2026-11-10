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
