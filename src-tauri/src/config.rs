use chrono::{DateTime, Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Window,
    Monitor,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Game {
    pub name: String,
    /// 进程名，按优先级排列：排在前面的窗口优先录（对局在前，客户端在后）。
    pub exes: Vec<String>,
    pub enabled: bool,
    /// None = 跟随全局设置
    #[serde(default)]
    pub scope: Option<Scope>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub games: Vec<Game>,
    pub scope: Scope,
    pub out_dir: PathBuf,
    /// 输出高度：480/720/1080/1440，0 = 跟随显示器
    pub resolution: u32,
    pub name_template: String,
    pub mic_enabled: bool,
    /// wasapi 设备 id，None = 系统默认麦克风
    pub mic_device: Option<String>,
    pub autostart: bool,
    /// 已按默认名保存、还等着用户确认命名的视频
    #[serde(default)]
    pub pending: Vec<PathBuf>,
    /// 正在录的会话目录；异常退出后靠它找回录制中途被改掉的旧保存目录里的录像
    #[serde(default)]
    pub recording: Option<PathBuf>,
}

pub const DEFAULT_TEMPLATE: &str = "${game}_${time:YYYY-MM-DD HH:mm:ss}";

fn game(name: &str, exes: &[&str]) -> Game {
    Game {
        name: name.into(),
        exes: exes.iter().map(|s| s.to_string()).collect(),
        enabled: true,
        scope: None,
    }
}

impl Config {
    pub fn new(out_dir: PathBuf) -> Self {
        Config {
            games: vec![
                game("LOL", &["League of Legends.exe", "LeagueClientUx.exe"]),
                game("无畏契约", &["VALORANT-Win64-Shipping.exe"]),
                game("三角洲行动", &["DeltaForceClient-Win64-Shipping.exe"]),
                game("Apex", &["r5apex.exe", "r5apex_dx12.exe"]),
            ],
            scope: Scope::Window,
            out_dir,
            resolution: 1080,
            name_template: DEFAULT_TEMPLATE.into(),
            mic_enabled: true,
            mic_device: None,
            autostart: true,
            pending: vec![],
            recording: None,
        }
    }

    /// 输出画布尺寸；monitor = 游戏所在显示器的分辨率（仅 resolution=0 时用到）
    pub fn canvas(&self, monitor: (u32, u32)) -> (u32, u32) {
        let even = |v: u32| v & !1;
        match self.resolution {
            0 => (even(monitor.0), even(monitor.1)),
            h => (even(h * 16 / 9), h),
        }
    }
}

/// 码率按像素数从 1080p=12Mbps 线性折算。
pub fn bitrate_kbps(w: u32, h: u32) -> u32 {
    let k = 12_000u64 * (w as u64 * h as u64) / (1920 * 1080);
    k.clamp(2_500, 60_000) as u32
}

/// 渲染命名模板：${game}、${time:dayjs 格式}；其余文字原样保留，最后替换非法字符。
pub fn render_name(tpl: &str, game: &str, t: DateTime<Local>) -> String {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            rest = "";
            break;
        };
        let var = &after[..j];
        if var == "game" {
            out.push_str(game);
        } else if let Some(fmt) = var.strip_prefix("time:") {
            out.push_str(&format_time(fmt, t));
        } else {
            out.push_str(&rest[i..i + 3 + j]);
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    sanitize(&out)
}

/// dayjs 常用 token 的子集；[...] 内为原样文字。
fn format_time(fmt: &str, t: DateTime<Local>) -> String {
    const TOKENS: [&str; 14] = ["YYYY", "YY", "MM", "M", "DD", "D", "HH", "H", "hh", "h", "mm", "m", "ss", "s"];
    let h12 = match t.hour() % 12 { 0 => 12, h => h };
    let mut out = String::new();
    let mut s = fmt;
    'outer: while !s.is_empty() {
        if let Some(r) = s.strip_prefix('[') {
            let end = r.find(']').unwrap_or(r.len());
            out.push_str(&r[..end]);
            s = r.get(end + 1..).unwrap_or("");
            continue;
        }
        for tok in TOKENS {
            if let Some(r) = s.strip_prefix(tok) {
                out.push_str(&match tok {
                    "YYYY" => format!("{:04}", t.year()),
                    "YY" => format!("{:02}", t.year() % 100),
                    "MM" => format!("{:02}", t.month()),
                    "M" => t.month().to_string(),
                    "DD" => format!("{:02}", t.day()),
                    "D" => t.day().to_string(),
                    "HH" => format!("{:02}", t.hour()),
                    "H" => t.hour().to_string(),
                    "hh" => format!("{:02}", h12),
                    "h" => h12.to_string(),
                    "mm" => format!("{:02}", t.minute()),
                    "m" => t.minute().to_string(),
                    "ss" => format!("{:02}", t.second()),
                    _ => t.second().to_string(),
                });
                s = r;
                continue 'outer;
            }
        }
        let c = s.chars().next().unwrap();
        out.push(c);
        s = &s[c.len_utf8()..];
    }
    out
}

/// Windows 文件名不能有半角冒号：冒号换成 -，其余非法字符换 _，去掉结尾的点和空格。
pub fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            ':' => '-',
            '\\' | '/' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    let s = s.trim().trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() { "recording".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn template() {
        let t = Local.with_ymd_and_hms(2026, 3, 7, 21, 5, 9).unwrap();
        assert_eq!(render_name(DEFAULT_TEMPLATE, "LOL", t), "LOL_2026-03-07 21-05-09");
        assert_eq!(render_name("${game} Delta ${time:YY.M.D h[点]}", "Apex", t), "Apex Delta 26.3.7 9点");
        assert_eq!(render_name("${foo}/${game", "X", t), "${foo}_${game");
        assert_eq!(render_name("  ...", "X", t), "recording");
        assert_eq!(bitrate_kbps(1920, 1080), 12_000);
        assert_eq!(Config::new(PathBuf::new()).canvas((3840, 2160)), (1920, 1080));
    }
}
