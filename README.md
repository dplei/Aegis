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

## 代码

- `src-tauri/src/recorder.rs`：轮询进程，开始、切换、结束录制。
- `src-tauri/src/config.rs`：配置、游戏预设、文件名模板。
- `src-tauri/src/win.rs`：进程、窗口、显示器、磁盘、回收站、麦克风列表。
- `src-tauri/src/main.rs`：托盘、窗口、前端命令。
- `ui/`：设置页和保存弹窗，原生 HTML，没有构建步骤。
