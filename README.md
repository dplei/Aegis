<p align="center">
  <img src="src-tauri/icons/128x128.png" width="88" alt="Aegis" />
</p>

<h1 align="center">Aegis</h1>

<p align="center">
  替你守着每一局。<br />
  <sub>A quiet shield for the games you play.</sub>
</p>

<p align="center">
  <a href="https://github.com/dplei/Aegis/releases/latest">下载</a> ·
  <a href="https://cdn.suibianwanwan.fun/aegis/aegis-setup.exe">国内下载</a> ·
  <a href="#开始">开始</a> ·
  <a href="#从源码构建">从源码构建</a>
</p>

<br />

Aegis，是神话里雅典娜那面盾的名字。

它不替你上场，只在旁边守着。游戏一打开，它就开始记；游戏一关，它轻轻问一句：这一局，要留下吗？

留下，就按你定的名字存好；不留，就放进回收站，反悔了还能捡回来。

## 它会做的事

**认得你的游戏。** 按进程名识别。英雄联盟、无畏契约、三角洲行动、Apex 已经放在列表里了，别的游戏开着的时候，从设置里点一下就能加进来。客户端和对局是两个窗口也没关系，进了对局它跟过去，回到大厅再跟回来，全都录在同一个文件里。

**只看游戏。** 用 Windows Graphics Capture 抓游戏窗口，被别的窗口挡住也照样录得到。不往游戏进程里注入任何东西，也不去读它的内存。

**声音跟着画面。** 录的是游戏自己的声音，微信、音乐这些不会混进来；麦克风可以开，录制中也能在托盘里一键静音。音画对齐在几毫秒以内。

**不拖慢游戏。** NVENC 硬件编码，画面从抓取到编码都在显存里。1080p 60 帧，每小时大约 5 GB。

**安静地待着。** 平时只是托盘里一个小图标；录制时，中间那点亮起红光。

## 开始

1. 从 [Releases](https://github.com/dplei/Aegis/releases/latest) 下载安装包，装好。
   国内下载：[aegis-setup.exe](https://cdn.suibianwanwan.fun/aegis/aegis-setup.exe)（七牛 CDN，始终是最新版）。
   签名还在申请中，Windows 可能弹出“已保护你的电脑”，点“更多信息”，再点“仍要运行”。
2. 打开 Aegis，在设置里勾上要录的游戏，选好保存目录。
3. 去玩吧。

需要 Windows 10 2004 或更新的系统，以及一块 NVIDIA 显卡。

### 文件名

默认是 `${game}_${time:YYYY-MM-DD HH:mm:ss}`，录出来像这样：

```
LOL_2026-11-10 21-55-02.mp4
```

`${game}` 是游戏名，`${time:…}` 是开始录制的时间，写法和 dayjs 一样。Windows 的文件名不能有冒号，所以冒号会换成 `-`。

## 从源码构建

外壳是 Tauri 2，录制交给 [libobs-rs](https://github.com/libobs-rs/libobs-rs)，也就是 OBS 32.1 的内核。

```bash
pnpm install
cargo install cargo-obs-build
cd src-tauri
cargo obs-build build --out-dir target/debug --remove-pdbs   # OBS 运行时，约 85 MB
cd ..
pnpm tauri dev
```

打包：

```bash
cd src-tauri && cargo obs-build build --out-dir ../obs-runtime --remove-pdbs && cd ..
pnpm tauri build    # 安装包在 src-tauri/target/release/bundle/nsis/
```

`obs-runtime/` 会作为资源装在 exe 旁边。打包前建议删掉 `obs-runtime/obs-plugins/64bit/rtmp-services.dll` 和 `obs-runtime/data/obs-plugins/rtmp-services/`，这个插件只给直播用，每次启动都会联网更新平台列表。

发版：改好 `src-tauri/tauri.conf.json` 里的 `version`，提交并推到 main，然后在 Actions 页运行 release，或者：

```bash
gh workflow run release.yml
```

[release.yml](.github/workflows/release.yml) 会在 Windows 上打包，自己打 `v<version>` 的 tag，把安装包发到 Releases，再镜像到七牛 CDN 供国内下载（需要仓库 secrets `QINIU_AK` / `QINIU_SK`）；这个版本已经发过时会直接失败。OBS 运行时的版本写在 `src-tauri/Cargo.toml` 的 `libobs-version`，按它缓存，平时发版不重新下载。

代码不多，六个文件：

| 文件 | 做什么 |
|---|---|
| `src-tauri/src/recorder.rs` | 盯着进程，开始、切换、结束录制 |
| `src-tauri/src/config.rs` | 配置、游戏预设、文件名模板 |
| `src-tauri/src/win.rs` | 进程、窗口、显示器、磁盘、回收站、启动文件夹快捷方式 |
| `src-tauri/src/main.rs` | 托盘、窗口、前端命令、开机自启、日志 |
| `src-tauri/src/export-logs.ps1` | 导出日志时打包日志、配置和系统信息 |
| `ui/` | 设置页和保存弹窗，原生 HTML |

## Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org).

The SignPath Foundation application is under review. Releases published before approval are unsigned.
（签名申请审核中，通过之前发布的安装包仍是未签名的。）

Team roles:

- Committers and reviewers: [dplei](https://github.com/dplei)
- Approvers: [dplei](https://github.com/dplei)

Only installers built automatically from this repository's source by GitHub Actions are signed. The bundled OBS Studio runtime consists of unmodified official OBS release files and is not signed by this project.
（只签本仓库源码经 GitHub Actions 自动构建的安装包；随包附带的 OBS 运行时是 OBS 官方原版文件，不由本项目签名。）

## Privacy

This program will not transfer any information to other networked systems unless specifically requested by the user or the person installing or operating it.

Aegis works fully offline: no uploads, no telemetry, no update checks. Recordings are stored only in the save folder you choose, settings only in `%APPDATA%\app.aegis.recorder`, and logs only in `%LOCALAPPDATA%\app.aegis.recorder\logs`; "Export logs" only writes a zip to your desktop for you to send yourself. The OBS `rtmp-services` plugin, the only OBS component that contacts the network (to update its streaming service list), is removed from release builds.
（Aegis 不联网：不上传、不统计、不检查更新。录像只存在你设置的保存目录，配置只存在 `%APPDATA%\app.aegis.recorder`，日志只存在 `%LOCALAPPDATA%\app.aegis.recorder\logs`；「导出日志」只是在桌面生成一个 zip，发不发、发给谁由你决定。打包时去掉了 OBS 里唯一会联网的直播插件 `rtmp-services`。）

## License

[GPL-3.0](LICENSE). The recording core, libobs, is GPL-licensed, so this project is released under GPL-3.0.
（录制内核 libobs 是 GPL 许可，本项目随之采用 GPL-3.0。）

<br />

<p align="center"><sub>精彩的一局，不该只活在记忆里。</sub></p>
