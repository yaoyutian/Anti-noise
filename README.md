# Anti-Noise

住宅楼上噪音的实时监测与取证工具。

用麦克风持续监听楼板撞击声（脚步、拖拽家具、掉落物），在事件发生时保留事件前后的录音和结构化记录；设计目标还包括半双工反击与失控保护。核心难点不在录音，而在**低频**：楼板撞击声的能量集中在 20–200 Hz，峰值在 63 Hz 八度带，而消费级麦克风、系统「音频增强」和通信模式都会优先削掉这一段。

> **当前状态：算法核心已完成，树莓派最小节点已跑通本机验证，桌面应用尚未接通。**
> 今天可以直接使用的是 **Rust 测试套件**、**`replay` 离线检测工具**，以及**树莓派无头最小节点 `noise-node`**。`noise-node` 已通过本机编译、单元测试和 3 秒音频流启动自检；尚未在树莓派、真实麦克风/喇叭和蓝牙音箱上验证。桌面监测界面仍在开发中。

## 当前状态

| 组件 | 状态 | 内容 |
| --- | --- | --- |
| `crates/noise-core` | 已实现 | 领域类型、单位、检测器与监测配置、事件定义 |
| `crates/noise-dsp` | 已实现 | Butterworth 滤波器设计、A/C/Z 计权、八度带、声级指标、麦克风校准 |
| `crates/noise-detect` | 已实现 | 自适应底噪、迟滞阈值、冲击状态机、半双工门控、离线特征提取与 `replay` 示例 |
| `crates/noise-audio` | 部分实现 | 设备枚举与参数协商、输入/输出流、无锁环、pre-roll 历史、回放波形、状态快照已完成；`analysis.rs` 尚未挂进 `lib.rs`，`engine.rs` 尚未落地 |
| `crates/noise-node` | 已实现（本机验证） | 无头节点：麦克风 → 检测 → 事件 WAV/JSONL → 延迟 3 秒原样回放；本机编译/单元测试/流启动通过，树莓派实机与真实声学链路尚未验证；见 [docs/raspberry-pi-minimal.md](docs/raspberry-pi-minimal.md) |
| `crates/noise-store` | 骨架 | 录音分段、事件索引、会话清单、保留策略、取证导出均未实现 |
| `src-tauri` | 部分实现 | Tauri 2 外壳，只有一个 `get_status` 命令；日志/单实例插件已在依赖中声明但未注册 |
| `src/`（前端） | 模板 | 仍是 `create-tauri-app` 模板；页面的 `greet` 命令在 Rust 侧不存在，按钮点击会报错 |

## 开发进度

最近更新：2026-11-25（分支 `feat/pi-minimal-node`）

### 已完成

- **树莓派无头最小节点 `noise-node`**：设备枚举/选择、20–200 Hz 触发、与 `replay` 同一条检测路径、pre-roll 环形历史、事件 WAV（32-bit float）+ `events.jsonl`、回放前裁剪首尾静音、输入/输出采样率不一致时线性重采样、半双工门控与 12 次/分钟速率限制、延迟原样回放。
- **音频流启动接口**：`InputBuild::play` / `OutputBuild::play`，避免节点直接依赖 cpal。
- **部署材料**：树莓派硬件选型、Pi 编译、systemd 与调参文档；示例单元 `deploy/noise-node.service`。
- **文档**：README、`docs/build.md`、`docs/usage.md` 与树莓派说明同步。

### 已验证

- `cargo check -p noise-node` 通过。
- `cargo test -p noise-node`：3 passed（静音裁剪、重采样）。
- `cargo test -p noise-audio -p noise-detect -p noise-dsp`：noise-audio 95、noise-detect 51 + 场景测试 12、noise-dsp 65 + 稳定性 9，全部通过。
- `noise-node --help` / `--list` 正常；本机枚举到 WASAPI 麦克风与扬声器。
- 3 秒实机流自检：48 kHz / 2 声道 / f32 的输入与输出流都能打开，分析线程正常运行。启动时出现一条 WASAPI buffer 警告，不影响运行。

### 尚未验证

- 树莓派 3B 实机（ARM 编译、ALSA、USB 麦克风/音箱）。
- 真实碰撞/拖凳子触发 → 录音 → 3 秒后回放的完整链路；本机自检只验证了流能打开。
- 蓝牙音箱 A2DP 延迟与 cpal/ALSA 可见性。
- 24 小时/7 天长时间浸泡、设备热插拔、磁盘保留策略。

### 下一步

1. 在 Pi 上用 `--list` 确认设备，先做 [docs/manual-test.md](docs/manual-test.md) 第 0 节的低频自检。
2. 用 `--run-for 120` 跑真实敲击，核对 WAV、JSONL 和回放。
3. 按现场误报/漏报调 `--min-rise`、`--no-lf-ratio`、`--hangover`。
4. 再决定是否启用自动回放与蓝牙。

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

**树莓派无头节点**：先列出设备，再指定输入/输出：

```bash
cargo run -p noise-node --release -- --list
cargo run -p noise-node --release -- --input <id> --output <id> --delay-ms 3000
```

硬件选型、Pi 上编译、systemd 和调参见 [docs/raspberry-pi-minimal.md](docs/raspberry-pi-minimal.md)。

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
| 列出树莓派节点可用设备 | `cargo run -p noise-node --release -- --list` |
| 启动树莓派最小节点 | `cargo run -p noise-node --release -- --input <id> --output <id> --delay-ms 3000` |
| 检查/测试树莓派节点 | `cargo check -p noise-node && cargo test -p noise-node` |
| 开发环境自检 | `npx tauri info` |

## 使用说明

**离线检测（现在的主力工具）**：把一段现场录音喂给与实时路径逐帧等价的提取器和检测状态机，打印事件、冲击、抑制原因和汇总统计。适合在硬件接入之前验证麦克风、录制方式和检测参数。

```bash
cargo run -p noise-detect --example replay -- 现场录音.wav
cargo run -p noise-detect --example replay -- 现场录音.wav --quiet
cargo run -p noise-detect --example replay -- 现场录音.wav --speed 1
```

参数含义、输出解读、录音准备和调参流程见 [docs/usage.md](docs/usage.md)。每个旋钮为什么存在、默认值被哪些实测推翻，见 [docs/detector-tuning.md](docs/detector-tuning.md)。

**树莓派最小节点**：`noise-node` 是一个无头程序，把麦克风、检测器、事件录音和喇叭串起来；检测到事件后延迟 3 秒把录到的声音原样放回去。WAV 保留完整 pre-roll，回放前只裁掉首尾静音。延迟从事件结束算起（含默认 1.5 s hangover），所以从物理声音到实际回放通常约 4–5 秒。它没有 UI、没有保留策略、也不处理设备热插拔，适合先把硬件链路和参数跑通，见 [docs/raspberry-pi-minimal.md](docs/raspberry-pi-minimal.md)。本机已验证流能打开，真实触发/回放仍待在 Pi 上验证。

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
| 麦克风 | ≥ 16 kHz，低频响应好 | 实机监测；USB 测量麦克风优先，树莓派上尚未实测 |
| 扬声器 | USB/有线优先 | `noise-node` 回放；蓝牙 A2DP 延迟与 ALSA 可见性需单独验证 |

平台安装命令、构建产物位置和常见编译错误见 [docs/build.md](docs/build.md)。

## 仓库结构

```text
crates/
  noise-core/    领域类型、单位、配置；零音频依赖
  noise-dsp/     纯 DSP：滤波器、计权、八度带、指标、校准
  noise-detect/  纯状态机：底噪、门限、冲击检测、门控、离线提取、replay 示例
  noise-store/   磁盘层（骨架）：录音分段、索引、会话、导出
  noise-audio/   音频引擎：设备、流、无锁环、pre-roll 历史、回放、状态
  noise-node/    树莓派无头最小节点：检测 → 事件 WAV → 延迟原样回放
src-tauri/       Tauri 2 桌面壳与构建配置
src/             React + TypeScript 前端
deploy/          树莓派 systemd 单元
docs/            声学推导、检测器调参、实机测试、树莓派部署
```

## 项目文档

| 文档 | 内容 |
| --- | --- |
| [docs/build.md](docs/build.md) | 安装工具链、平台依赖、编译、打包、测试与故障排查 |
| [docs/usage.md](docs/usage.md) | 当前可用功能、replay 工具用法、录音准备、校准、实机前设置 |
| [docs/raspberry-pi-minimal.md](docs/raspberry-pi-minimal.md) | 树莓派最小节点：麦克风/喇叭选型、Pi 编译、systemd、调参 |
| [docs/detector-tuning.md](docs/detector-tuning.md) | 检测判据的推导与被实测推翻的假设，旋钮清单与调参回路 |
| [docs/acoustics-notes.md](docs/acoustics-notes.md) | A 计权、时间计权、校准、八度带、反馈啸叫等声学推导 |
| [docs/manual-test.md](docs/manual-test.md) | 必须有真实麦克风、音响和房间才能验证的硬件在环清单 |

## 已知限制

- **没有校准就不能声称绝对声压级。** 未校准时所有读数都是 dBFS；`Calibration::dbfs_to_dbspl` 返回 `Option` 是刻意的。
- **八度带是近似，不是合规测量。** 滤波器裙边比 IEC 61260 Class 1 缓，不能用于出具合规频谱报告。
- **实时采集要求采样率 ≥ 16 kHz。** 蓝牙耳机的 HFP 模式（8/16 kHz）会同时破坏录制和回放；`noise-audio` 会拒绝在更低采样率下启动。
- **麦克风低频滚降是本项目的头号风险。** 50 Hz 的响应远弱于 1 kHz 时，先换传感器，再谈调参。
- **桌面应用不会写录音或证据文件。** `noise-store` 尚未实现；`tauri.conf.json` 中的 `$APPDATA/anti-noise/sessions/**` 只是为将来的播放协议预留的路径。
- **`noise-node` 会写事件 WAV 和 JSONL，但没有保留策略、热插拔重连或校准。** 事件多时磁盘会一直增长；USB 设备掉线后需要重启服务。
- **`noise-node` 目前只在 Windows 本机验证了编译、单元测试和流启动；树莓派、真实声学链路和蓝牙回放尚未验证。**
- **桌面界面尚未接通。** 安装包可以构建，但打开后只有模板界面。
- **安装包未签名。** Windows SmartScreen 和 macOS Gatekeeper 会拦截；正式分发需要签名与公证。

## License

MIT（见根目录 `Cargo.toml` 的 `workspace.package.license`）。
