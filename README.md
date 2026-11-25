# Anti-Noise

住宅楼上噪音的实时监测与取证工具。

用麦克风持续监听楼板撞击声（脚步、拖拽家具、掉落物），在事件发生时保留事件前后的录音和结构化记录；设计目标还包括半双工反击与失控保护。核心难点不在录音，而在**低频**：楼板撞击声的能量集中在 20–200 Hz，峰值在 63 Hz 八度带，而消费级麦克风、系统「音频增强」和通信模式都会优先削掉这一段。

> **当前状态：算法核心已完成，桌面应用尚未接通。**
> 今天可以直接使用的是 **Rust 测试套件**和 **`replay` 离线检测工具**；实时采集、录音落盘和监测界面仍在开发中。

## 当前状态

| 组件 | 状态 | 内容 |
| --- | --- | --- |
| `crates/noise-core` | 已实现 | 领域类型、单位、检测器与监测配置、事件定义 |
| `crates/noise-dsp` | 已实现 | Butterworth 滤波器设计、A/C/Z 计权、八度带、声级指标、麦克风校准 |
| `crates/noise-detect` | 已实现 | 自适应底噪、迟滞阈值、冲击状态机、半双工门控、离线特征提取与 `replay` 示例 |
| `crates/noise-audio` | 部分实现 | 设备枚举与参数协商、输入/输出流、无锁环、pre-roll 历史、回放波形、状态快照已完成；`analysis.rs` 尚未挂进 `lib.rs`，`engine.rs` 尚未落地 |
| `crates/noise-store` | 骨架 | 录音分段、事件索引、会话清单、保留策略、取证导出均未实现 |
| `src-tauri` | 部分实现 | Tauri 2 外壳，只有一个 `get_status` 命令；日志/单实例插件已在依赖中声明但未注册 |
| `src/`（前端） | 模板 | 仍是 `create-tauri-app` 模板；页面的 `greet` 命令在 Rust 侧不存在，按钮点击会报错 |

## 快速开始

前置要求：Rust **1.94.0**、Node.js **^20.19 或 >=22.12**；Windows 还需要 C++ 生成工具。完整清单和平台差异见 [docs/build.md](docs/build.md)。

以下命令都在仓库根目录执行：

```bash
# 1. 安装前端依赖
npm install

# 2. 跑算法测试（不需要声卡）
cargo test -p noise-core -p noise-dsp -p noise-detect

# 3. 用一段 WAV 跑离线检测
cargo run -p noise-detect --example replay -- 录音.wav

# 4. 启动桌面应用（当前只到模板界面，不能监测）
npm run tauri dev
```

第 3 条命令里的 `--` 是 cargo 的参数分隔符，不能省略；路径有空格时用引号包起来。

## 常用命令

| 目的 | 命令 |
| --- | --- |
| 安装前端依赖 | `npm install`（或按 lockfile 复现：`npm ci`） |
| 启动桌面应用（开发模式） | `npm run tauri dev` |
| 只启动前端（浏览器，无 Tauri IPC） | `npm run dev` |
| 构建安装包 | `npm run tauri build` |
| 全工作区测试（会编译 Tauri 壳） | `cargo test --workspace` |
| 不依赖 Tauri 的算法测试 | `cargo test -p noise-core -p noise-dsp -p noise-detect -p noise-audio` |
| 离线检测一段录音 | `cargo run -p noise-detect --example replay -- <wav>` |
| 查看 replay 全部参数 | `cargo run -p noise-detect --example replay -- --help` |
| 开发环境自检 | `npx tauri info` |

## 使用说明

**离线检测（现在的主力工具）**：把一段现场录音喂给与实时路径逐帧等价的提取器和检测状态机，打印事件、冲击、抑制原因和汇总统计。适合在硬件接入之前验证麦克风、录制方式和检测参数。

```bash
cargo run -p noise-detect --example replay -- 现场录音.wav
cargo run -p noise-detect --example replay -- 现场录音.wav --quiet
cargo run -p noise-detect --example replay -- 现场录音.wav --speed 1
```

参数含义、输出解读、录音准备和调参流程见 [docs/usage.md](docs/usage.md)。每个旋钮为什么存在、默认值被哪些实测推翻，见 [docs/detector-tuning.md](docs/detector-tuning.md)。

**桌面应用**：`npm run tauri dev` 会打开标题为「Anti-Noise — 楼上噪音监测」的窗口，但界面仍是 Tauri 模板，且没有接入音频引擎。当前唯一可用的 IPC 命令是 `get_status`，它固定返回 `stopped`。

**实机测试之前**：先做 [docs/manual-test.md](docs/manual-test.md) 第 0 节的低频自检。如果麦克风在 50 Hz 附近严重滚降，后面的算法和调参都不会有意义。

## 环境要求

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| Rust | 1.94.0（`rust-toolchain.toml` 锁定） | 全部 Rust crate |
| Node.js | `^20.19.0` 或 `>=22.12.0` | 前端构建与 Tauri CLI（Vite 8 的要求） |
| npm | 随 Node | 安装前端依赖 |
| MSVC C++ 生成工具 | VS 2022 Build Tools | Windows 上编译 Rust（`x86_64-pc-windows-msvc`） |
| WebView2 | Windows 11 自带 | Tauri 运行时；Windows 10 需安装 Evergreen Runtime |
| webkit2gtk 4.1 / ALSA 开发包 | 见文档 | Linux 上编译 Tauri 与 cpal |
| 麦克风 | ≥ 16 kHz，低频响应好 | 实机监测（当前尚未接通） |

平台安装命令、构建产物位置和常见编译错误见 [docs/build.md](docs/build.md)。

## 仓库结构

```text
crates/
  noise-core/    领域类型、单位、配置；零音频依赖
  noise-dsp/     纯 DSP：滤波器、计权、八度带、指标、校准
  noise-detect/  纯状态机：底噪、门限、冲击检测、门控、离线提取、replay 示例
  noise-store/   磁盘层（骨架）：录音分段、索引、会话、导出
  noise-audio/   音频引擎：设备、流、无锁环、pre-roll 历史、回放、状态
src-tauri/       Tauri 2 桌面壳与构建配置
src/             React + TypeScript 前端
docs/            声学推导、检测器调参、实机测试清单
```

## 项目文档

| 文档 | 内容 |
| --- | --- |
| [docs/build.md](docs/build.md) | 安装工具链、平台依赖、编译、打包、测试与故障排查 |
| [docs/usage.md](docs/usage.md) | 当前可用功能、replay 工具用法、录音准备、校准、实机前设置 |
| [docs/detector-tuning.md](docs/detector-tuning.md) | 检测判据的推导与被实测推翻的假设，旋钮清单与调参回路 |
| [docs/acoustics-notes.md](docs/acoustics-notes.md) | A 计权、时间计权、校准、八度带、反馈啸叫等声学推导 |
| [docs/manual-test.md](docs/manual-test.md) | 必须有真实麦克风、音响和房间才能验证的硬件在环清单 |

## 已知限制

- **没有校准就不能声称绝对声压级。** 未校准时所有读数都是 dBFS；`Calibration::dbfs_to_dbspl` 返回 `Option` 是刻意的。
- **八度带是近似，不是合规测量。** 滤波器裙边比 IEC 61260 Class 1 缓，不能用于出具合规频谱报告。
- **实时采集要求采样率 ≥ 16 kHz。** 蓝牙耳机的 HFP 模式（8/16 kHz）会同时破坏录制和回放；`noise-audio` 会拒绝在更低采样率下启动。
- **麦克风低频滚降是本项目的头号风险。** 50 Hz 的响应远弱于 1 kHz 时，先换传感器，再谈调参。
- **当前不会写任何录音或证据文件。** `noise-store` 尚未实现；`tauri.conf.json` 中的 `$APPDATA/anti-noise/sessions/**` 只是为将来的播放协议预留的路径。
- **桌面界面尚未接通。** 安装包可以构建，但打开后只有模板界面。
- **安装包未签名。** Windows SmartScreen 和 macOS Gatekeeper 会拦截；正式分发需要签名与公证。

## License

MIT（见根目录 `Cargo.toml` 的 `workspace.package.license`）。
