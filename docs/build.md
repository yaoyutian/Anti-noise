# 安装与编译

本文说明如何从源码安装工具链、编译、运行、打包和测试 Anti-Noise。

当前仓库的桌面应用只到 Tauri 模板界面；**构建成功不等于能监测**。构建完成后实际可用的功能见 [usage.md](usage.md)。

---

## 0. 版本要求

| 依赖 | 要求 | 说明 |
| --- | --- | --- |
| Rust | **1.94.0** | `rust-toolchain.toml` 锁定了精确版本并带 `rustfmt`、`clippy`；`Cargo.toml` 的 MSRV 也是 1.94 |
| Node.js | `^20.19.0` 或 `>=22.12.0` | Vite 8 的 `engines` 要求（见 `package-lock.json`） |
| npm | 随 Node | 安装前端依赖 |
| Tauri CLI | 2.x | 已是 devDependency，由 `npm install` 安装，无需全局安装 |
| 操作系统 | Windows 10/11、macOS 10.15+、主流 Linux | 项目按桌面平台设计，不支持移动端 |

**不要用发行版仓库里的 `rustc`/`cargo`。** 版本通常太旧，且不会读取 `rust-toolchain.toml`。用 [rustup](https://rustup.rs/) 安装。

---

## 1. 安装 Rust

1. 安装 rustup（Windows 下载 `rustup-init.exe`，macOS/Linux 用官网脚本）。
2. 在仓库目录执行任意 `cargo` 命令时，rustup 会读到 `rust-toolchain.toml`，自动安装 `1.94.0` 及 `rustfmt`、`clippy`。
3. 验证：

```bash
rustup show
rustc -V
```

期望看到 `rustc 1.94.0`，host 在 Windows 上是 `x86_64-pc-windows-msvc`。

若自动安装失败，可手动安装：

```bash
rustup toolchain install 1.94.0 --component rustfmt clippy
```

---

## 2. Windows

### 2.1 C++ 生成工具

安装 [Visual Studio 2022 Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)，勾选工作负载：

- **使用 C++ 的桌面开发**

该工作负载包含 MSVC v143 编译器、链接器和 Windows SDK，Rust 的 `*-msvc` 目标依赖它们。安装完成后重新打开终端。

验证（在「x64 Native Tools Command Prompt for VS 2022」或普通新终端中）：

```bat
where link.exe
```

能定位到 MSVC 的 `link.exe` 即可。若报 `link.exe not found` / `LNK1104`，见[常见问题](#8-常见问题)。

### 2.2 WebView2

Windows 11 自带 WebView2 Runtime，无需额外安装。Windows 10 若没有，安装 [Evergreen Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)。

只跑测试不需要 WebView2；运行桌面应用需要。

### 2.3 麦克风权限

后续接通实时采集后，需要允许桌面应用访问麦克风：

设置 → 隐私和安全性 → 麦克风 → 允许桌面应用访问麦克风。

当前版本不会请求麦克风，这一步暂时不影响构建和测试。

### 2.4 不要用 WSL

Tauri 的 Linux 构建链、音频设备直通和 WebView2 在 WSL 下都有额外限制。除非你明确要构建 Linux 包，否则请在原生 Windows 上开发。

---

## 3. macOS

```bash
xcode-select --install
```

- Tauri 配置的最低系统版本是 macOS 10.15（`tauri.conf.json` 的 `bundle.macOS.minimumSystemVersion`）。
- Apple Silicon 默认使用 `aarch64-apple-darwin`，不需要 Rosetta。
- **接通麦克风之前**，`Info.plist` 必须包含 `NSMicrophoneUsageDescription`，否则系统会直接拒绝采集。当前 `tauri.conf.json` 还没有这一项；实现麦克风功能时要补上并重新签名。
- 分发给他人需要 Apple Developer 签名与公证，当前未配置。

---

## 4. Linux

Tauri 2 要求 **webkit2gtk 4.1**（不是 4.0）。cpal 在 Linux 上通过 ALSA 采集，还需要 ALSA 开发包。

### Debian / Ubuntu（22.04+）

```bash
sudo apt update
sudo apt install -y \
  build-essential curl wget file pkg-config \
  libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev \
  libasound2-dev
```

Ubuntu 20.04 只有 webkit2gtk 4.0，不满足 Tauri 2 的要求，请升级系统。

### Fedora

```bash
sudo dnf group install -y "c-development"
sudo dnf install -y \
  curl wget file pkgconf-pkg-config \
  webkit2gtk4.1-devel openssl-devel \
  libappindicator-gtk3-devel librsvg2-devel \
  alsa-lib-devel
```

### Arch Linux

```bash
sudo pacman -S --needed \
  base-devel curl wget file pkgconf \
  webkit2gtk-4.1 openssl \
  libappindicator-gtk3 librsvg xdotool \
  alsa-lib
```

---

## 5. 获取源码与前端依赖

```bash
git clone <repo-url> Anti-noise
cd Anti-noise

node -v     # 需 ^20.19.0 或 >=22.12.0
npm -v

npm ci      # 严格按 package-lock.json 安装
# 或 npm install
```

`npm install` 还会把 `@tauri-apps/cli` 装进 `node_modules`，下面的 `npm run tauri ...` 和 `npx tauri ...` 都使用这个本地 CLI。

---

## 6. 构建与运行

### 6.1 开发模式

```bash
npm run tauri dev
```

这条命令会：

1. 执行 `beforeDevCommand`（`npm run dev`），Vite 在 `http://localhost:1420` 启动；
2. 编译 Rust（debug 配置）并打开桌面窗口。

Vite 的端口是 `strictPort: true`，1420 被占用时会直接失败，不会自动换端口。

首次编译会比较慢：根 `Cargo.toml` 把**所有依赖**的 dev 配置设为 `opt-level = 2`，自己的代码保留调试信息。这不是笔误——未优化的 DSP 在 debug 构建下会让实时音频路径掉帧。

只改前端样式时可以只跑：

```bash
npm run dev
```

浏览器能打开界面，但 Tauri IPC（`invoke`）不可用，适合调布局，不适合验证功能。

### 6.2 生产构建

```bash
npm run tauri build
```

流程是 `tsc && vite build`（产出 `dist/`），再按 release 配置编译 Rust：`lto = true`、`codegen-units = 1`、`opt-level = 3`、`strip = true`、`panic = "abort"`。因此**发布构建明显更慢、二进制更小**。

产物位置（Cargo workspace 的 target 目录在仓库根）：

| 平台 | 可执行文件 | 安装包 |
| --- | --- | --- |
| Windows | `target/release/anti-noise.exe` | `target/release/bundle/nsis/`、`target/release/bundle/msi/` |
| macOS | `target/release/anti-noise` | `target/release/bundle/dmg/`、`target/release/bundle/macos/` |
| Linux | `target/release/anti-noise` | `target/release/bundle/deb/`、`rpm/`、`appimage/` |

`tauri.conf.json` 里 `bundle.targets = "all"`，具体产出取决于平台和打包工具是否可用。首次打包时 Tauri 可能联网下载 NSIS/WiX（Windows）或 linuxdeploy（Linux）。

常用变体：

```bash
# 调试版安装包：保留符号、编译更快
npx tauri build --debug

# 只打某一种包（示例：Windows NSIS）
npx tauri build --bundles nsis
```

### 6.3 安装构建产物

- **Windows**：运行 `bundle/nsis/*-setup.exe`，或 `msiexec /i bundle/msi/*.msi`。安装包未签名，SmartScreen 会提示「未知发布者」，本地测试可选择「仍要运行」。
- **macOS**：打开 `.dmg`，把应用拖进「应用程序」。未签名/未公证的应用会被 Gatekeeper 拦截；本地测试可右键 → 打开，或 `xattr -dr com.apple.quarantine /Applications/Anti-Noise.app`。这不是分发给他人时的解决方案。
- **Linux**：`sudo apt install ./target/release/bundle/deb/*.deb`；Fedora 用 `sudo dnf install ./...rpm`；AppImage `chmod +x` 后直接运行。

再次提醒：当前安装出来的应用只有模板界面，不会录音，也不会监测。

---

## 7. 测试

| 目的 | 命令 | 依赖 |
| --- | --- | --- |
| 全工作区测试 | `cargo test --workspace` | 会编译 `src-tauri` 和 `noise-node`，需要 Tauri 平台依赖与 ALSA |
| 纯算法测试（不含 Tauri） | `cargo test -p noise-core -p noise-dsp -p noise-detect -p noise-audio` | Linux 需要 ALSA 开发包；不需要声卡 |
| 最小无音频依赖子集 | `cargo test -p noise-core -p noise-dsp -p noise-detect` | 无 |
| 检测场景套件 | `cargo test -p noise-detect --test trigger_scenarios` | 无 |
| DSP 稳定性测试 | `cargo test -p noise-dsp --test stability` | 无 |
| 单条回归测试 | `cargo test -p noise-dsp frame_crest_cannot_separate_rumble_from_impulse_but_rise_can -- --nocapture` | 无 |
| 实时分配守卫 | `cargo test -p noise-audio --features rt-guard` | debug 构建才生效 |
| 编译树莓派无头节点 | `cargo build --release -p noise-node` | Linux 需要 ALSA 开发包；不需要声卡即可编译 |

说明：

- 测试**不需要真实麦克风**。硬件相关项目在 [manual-test.md](manual-test.md)，必须人工执行。
- `rt-guard` 挂载计数全局分配器，只在 `debug_assertions` 且启用 feature 时记账；`--release` 下加了也不生效，这是刻意的。
- `replay` 示例不需要手动打开 `testgen` feature：`noise-detect::offline` 是无条件编译的，`testgen` 只供场景测试和合成信号使用。

树莓派上的无头节点（`noise-node`）另有硬件选型、交叉编译和 systemd 说明，见 [raspberry-pi-minimal.md](raspberry-pi-minimal.md)。
- 提交前建议：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
```

---

## 8. 常见问题

| 现象 | 原因与处理 |
| --- | --- |
| `link.exe not found` / `LNK1104` | 没装 MSVC C++ 工作负载；装好后重开终端 |
| Tauri 窗口空白或启动失败，提示 WebView2 | 安装 Evergreen WebView2 Runtime |
| `webkit2gtk-4.1` 找不到 | 装 `libwebkit2gtk-4.1-dev`（不是 4.0）；确认 `pkg-config --modversion webkit2gtk-4.1` |
| `The system library alsa was not found` | Debian/Ubuntu 装 `libasound2-dev`，Fedora 装 `alsa-lib-devel`，Arch 装 `alsa-lib` |
| `failed to run custom build command`（Tauri/GTK） | 平台开发包不全，对照第 4 节补齐后 `cargo clean -p <crate>` 重试 |
| Vite 报端口 1420 被占用 | 释放端口，或同时修改 `vite.config.ts` 的 `server.port` 与 `tauri.conf.json` 的 `build.devUrl` |
| Node 报版本不支持 | 升级到 Node 20.19+ 或 22.12+；`node -v` 确认当前 shell 用的版本 |
| `rustc` 版本不是 1.94.0 | 用 rustup；`rustup show` 查看当前目录生效的 toolchain |
| 首次构建非常慢 | 正常：依赖 `opt-level = 2`，release 还开 LTO；后续增量构建会快很多 |
| `cargo test --workspace` 在 Tauri 处失败 | 先跑纯算法子集；或按平台安装 Tauri 依赖 |
| 安装包被 SmartScreen / Gatekeeper 拦截 | 未签名，属预期；正式分发需签名/公证 |
| `replay` 打不开录音 | 只支持 WAV（PCM 整型或 IEEE float）；m4a/aac/mp3 先转 WAV，见 [usage.md](usage.md) |

---

## 9. 构建自检

按顺序确认：

```bash
rustc -V
node -v
npx tauri info
cargo metadata --no-deps --format-version 1
cargo run -p noise-detect --example replay -- --help
```

期望：

- `rustc` 为 1.94.0；
- Node 满足 `^20.19.0 || >=22.12.0`；
- `tauri info` 列出各平台依赖且没有缺失项；
- `cargo metadata` 的 `workspace_members` 恰好 **7** 个成员：`src-tauri` 加 6 个 `crates/*`。`src-tauri/Cargo.toml` 顶部注释特别提醒过：`create-tauri-app` 的某些版本会写入嵌套 workspace，静默吞掉根 crate 图，导致 `cargo test --workspace` 什么都不测；
- `replay --help` 打印用法（它输出到 stderr，退出码 0）。

---

## 10. 交叉编译

项目没有配置交叉编译。Tauri 应用依赖目标平台的 WebView 与打包工具，**请在目标操作系统上构建**：

- Windows 包在 Windows 上构建；
- macOS 包在 macOS 上构建；
- Linux 包在对应发行版上构建。

CI 或虚拟机中构建时，同样要先装好对应平台的系统依赖。
