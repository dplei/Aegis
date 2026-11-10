# Aegis

游戏一打开就自动录，游戏关掉时问要不要留、叫什么名字。Windows + NVIDIA 显卡。

录制内核是 [libobs-rs](https://github.com/libobs-rs/libobs-rs)（OBS 32.1），外壳是 Tauri 2：

- 画面：WGC 抓游戏窗口（不注入进程），或者抓游戏所在的整个显示器。
- 声音：OBS 的应用音频捕获，跟着当前录的窗口走；另外可选麦克风。
- 编码：NVENC HEVC，纹理编码，画面全程在显存里；输出分片 MP4。

## 开发

```bash
pnpm install
cargo install cargo-obs-build
cd src-tauri
cargo obs-build build --out-dir target/debug --remove-pdbs   # OBS 运行时，约 85MB
cd ..
pnpm tauri dev
```

## 打包

```bash
cd src-tauri && cargo obs-build build --out-dir ../obs-runtime --remove-pdbs && cd ..
pnpm tauri build    # 安装包在 src-tauri/target/release/bundle/nsis/
```

`obs-runtime/` 会作为资源装到 exe 旁边。打包前建议删掉 `obs-runtime/obs-plugins/64bit/rtmp-services.dll` 和 `obs-runtime/data/obs-plugins/rtmp-services/`：这个插件只给直播用，每次启动都会联网更新平台列表。

## 发版

改好 `src-tauri/tauri.conf.json` 里的 `version` 并提交，然后推同名 tag：

```bash
git tag v0.1.0
git push origin v0.1.0
```

`.github/workflows/release.yml` 会在 Windows 上打包，把安装包发到 GitHub Release。tag 和 `version` 不一致时会直接失败。

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

Aegis works fully offline: no uploads, no telemetry, no update checks. Recordings are stored only in the save folder you choose, and settings only in `%APPDATA%\app.aegis.recorder`. The OBS `rtmp-services` plugin, the only OBS component that contacts the network (to update its streaming service list), is removed from release builds.
（Aegis 不联网：不上传、不统计、不检查更新。录像只存在你设置的保存目录，配置只存在 `%APPDATA%\app.aegis.recorder`。打包时去掉了 OBS 里唯一会联网的直播插件 `rtmp-services`。）

## License

[GPL-3.0](LICENSE). The recording core, libobs, is GPL-licensed, so this project is released under GPL-3.0.
（录制内核 libobs 是 GPL 许可，本项目随之采用 GPL-3.0。）

## 代码

- `src-tauri/src/recorder.rs`：轮询进程，开始、切换、结束录制。
- `src-tauri/src/config.rs`：配置、游戏预设、文件名模板。
- `src-tauri/src/win.rs`：进程、窗口、显示器、磁盘、回收站、麦克风列表。
- `src-tauri/src/main.rs`：托盘、窗口、前端命令。
- `ui/`：设置页和保存弹窗，原生 HTML，没有构建步骤。
