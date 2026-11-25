# 树莓派最小节点（麦克风 + 喇叭）

本文对应 `crates/noise-node`：一个**无头**最小系统，把「麦克风 → 检测 → 录 WAV → 延迟后原样回放」这条物理链路先跑通。

它不是完整产品：没有 UI、没有会话分段/保留策略、没有热插拔重连。完整边界见文末。

> 以下硬件建议是通用经验，不是针对某一款产品的实测结论；麦克风请先按 [manual-test.md](manual-test.md) 第 0 节做低频自检。

---

## 1. 信号链

```text
USB 麦克风 / I2S 麦克风
        │
        ▼
   ALSA（Raspberry Pi OS）
        │
        ▼
 noise-audio 输入流（单声道 f32，无锁环）
        │
        ▼
 20–200 Hz 触发带 + 八度带 + 帧特征
        │
        ▼
 noise-detect 状态机（与 replay 同一条路径）
        │
        ├─ 事件结束 → 从 pre-roll 历史取音频 → events/event-*.wav + events.jsonl
        │
        └─ 半双工门控 → 延迟 3 s → 输出流 → USB/有线音箱
```

回放的是**录到的声音本身**（去掉首尾静音），不是 `response.rs` 里的 55 Hz 合成闷响。WAV 里保留完整 pre-roll，方便事后核对。

---

## 2. Pi 3B 能不能用

能，但只建议做**无头**节点：

| 项目 | 结论 |
| --- | --- |
| CPU | 4×Cortex-A53 @1.2 GHz；这套 DSP 只有几十个双二阶滤波器，不是瓶颈 |
| 内存 | 1 GB 跑 Raspberry Pi OS **Lite 64-bit** 足够；不要装桌面 |
| 实时性 | 普通 Linux 即可；门控和环缓冲已经按 10–30 ms 缓冲设计 |
| 时钟 | **Pi 3B 没有 RTC**；联网靠 NTP，离线取证建议加 DS3231 之类的 RTC |
| 存储 | 事件录音写 USB SSD 或高耐久 SD；不要用廉价卡长期写 |
| 无线 | Wi-Fi 和蓝牙共用天线；用蓝牙音箱时优先走网线，或关 Wi-Fi |
| 供电 | 官方 5 V 2.5 A；供电不足首先表现为 USB 音频断流 |
| 音频输出 | 3.5 mm 只有输出，没有输入；音质一般，优先 USB 声卡 |

如果还没买板子：纯无头 3B 够用；想以后跑 UI/多服务，直接上 Pi 4/5 2 GB+ 更省事。

---

## 3. 麦克风：这是成败点

**不要用 KY-038 / LM393 这类「声音传感器」模块。** 它们通常只有数字阈值或包络输出，拿不到 20–200 Hz 波形；Pi 3B 也没有模拟输入，模拟输出需要额外 ADC。它们可以做粗唤醒，替代不了麦克风。

| 方案 | 20–200 Hz | 接入 Pi | 评价 |
| --- | --- | --- | --- |
| USB 测量麦克风（miniDSP UMIK-1/2 等） | 好，通常 20 Hz 起 | USB，即插即用 | **首选**；有校准文件，低频响应明确 |
| USB 声卡 + 电容/驻极体麦 | 取决于麦克风 | USB | 便宜；必须实测低频，避开带 AGC/降噪的声卡 |
| I2S MEMS（INMP441、ICS-43434 等） | 常见下限约 60 Hz | I2S overlay | 便宜、体积小；63 Hz 正好在边缘，不推荐作为首选 |
| 压电接触麦克风 + 高阻前放 | 低频好 | 需 ADC/USB 声卡 | 贴天花板测结构声；判据和校准要重新验证 |
| 模拟声音传感器模块 | 差/只有包络 | 需 ADC，不能直插 3.5 mm | **不适合本项目** |

先做自检：手机播放 50 Hz 与 1 kHz 正弦，录下来用 Audacity 看频谱，63 Hz 附近的落差应小于 20 dB。具体步骤见 [manual-test.md](manual-test.md) 第 0 节。自检不过，换麦克风/位置，不要先调检测器。

---

## 4. 喇叭：先有线，蓝牙最后

**首选 USB 小音箱 / USB 声卡 + 有源音箱。** Pi 3B 的 3.5 mm 输出底噪和低频都一般；I2S 功放（如 MAX98357A）也可以，但需要焊接和配置。

蓝牙音箱不是不能用，但要注意：

- **A2DP 延迟通常上百毫秒**，具体看编码器和音箱缓冲。门控按「回放时长」计时，不知道蓝牙延迟；声音可能比预期晚到，甚至门控恢复后尾音还在响。
- 蓝牙音箱通常要经过 PipeWire / PulseAudio / BlueALSA 才暴露成 ALSA 设备；cpal 在 Linux 走 ALSA，**不一定能直接枚举到蓝牙 sink**。先用 `--list` 看；看不到就先把音箱设成系统默认 sink，再让 `--output` 用默认设备，或改用有线。
- Pi 3B 的 Wi-Fi/蓝牙共存较差；用蓝牙时尽量走网线。
- 小音箱对 50–100 Hz 的还原有限；本节点回放的是原始录音（含谐波），比纯 55 Hz 合成声更容易听出来，但仍要实测。

**第一次实机测试请用有线/USB 音箱。** 蓝牙等检测和录音稳定后再单独测延迟。

---

## 5. 在 Pi 上编译

### 5.1 系统依赖

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libasound2-dev
```

`libasound2-dev` 是 cpal/ALSA 的编译依赖。

### 5.2 安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

仓库的 `rust-toolchain.toml` 会拉取 1.94.0。

### 5.3 编译

```bash
cd ~/Anti-noise
cargo build --release -p noise-node
```

Pi 3B 只有 1 GB 内存，release 的 LTO 可能很慢或吃紧。可以临时加大 swap：

```bash
sudo dphys-swapfile swapoff
sudo sed -i 's/^CONF_SWAPSIZE=.*/CONF_SWAPSIZE=2048/' /etc/dphys-swapfile
sudo dphys-swapfile setup
sudo dphys-swapfile swapon
```

也可以先用非 LTO 配置把流程跑通：

```bash
CARGO_PROFILE_RELEASE_LTO=false cargo build --release -p noise-node
```

或者在 PC 上用 Docker + `cross` 交叉编译到 `aarch64-unknown-linux-gnu`；交叉镜像需要自己补 `libasound2-dev`，比在 Pi 上直接编译多一步配置。**不要尝试在 Pi 3B 上编译 Tauri 桌面版。**

---

## 6. 运行

```bash
./target/release/noise-node --list
```

拿到输入/输出设备的 id 或名字后：

```bash
./target/release/noise-node \
  --input "USB PnP Sound Device" \
  --output "USB PnP Sound Device" \
  --delay-ms 3000 \
  --frames-dir /mnt/ssd/noise-events
```

名字有空格要加引号；名字不唯一时用 `--list` 打印的 id。

第一次建议加 `--run-for 120` 跑两分钟，敲一下地板/桌子：

```bash
./target/release/noise-node --input "..." --output "..." --run-for 120
```

期望在日志里看到：

```text
[...] 事件开始 #1 ...
[...]   └ 冲击 #1 ...
[...] 事件结束 #1 类型 Impact ...
  已排定 3.0s 后回放（0.8s，第 1 次）
[回放] 事件 #1 开始
[回放] 事件 #1 结束
```

并在 `--frames-dir` 下得到 `event-0001-*.wav` 和 `events.jsonl`。

### 6.1 常用调参

| 现象 | 先试 |
| --- | --- |
| 空调/车流也触发 | `--min-rise 20 --impact-only`，或 `--max-event-ms 6000` |
| 拖凳子的持续摩擦没触发 | `--min-rise 10 --hangover 2500 --lf-ratio 3`；再不行试 `--no-lf-ratio` |
| 只有巨大碰撞、没有拖动 | 正常：拖动可能被归为 `Sustained`；不要加 `--impact-only` |
| 回放太响/啸叫 | `--gain-db -6`；把音箱挪离麦克风；降低音量 |
| 事件太长被跳过 | 适当调大 `--max-event-ms`，同时注意历史缓冲和磁盘 |
| 想只录不回放 | 先把 `--gain-db -80`，或把 `--output` 指到一个不出声的设备；本版本没有单独的 `--no-playback` |

所有可用参数见 `noise-node --help`。检测判据的推导见 [detector-tuning.md](detector-tuning.md)。

---

## 7. 开机自启

示例单元在 [`deploy/noise-node.service`](../deploy/noise-node.service)：

```bash
sudo useradd --system --user-group --groups audio --home /var/lib/noise-node --shell /usr/sbin/nologin antinoise
sudo install -m 755 target/release/noise-node /usr/local/bin/noise-node
sudo install -m 644 deploy/noise-node.service /etc/systemd/system/noise-node.service
sudo systemctl daemon-reload
sudo systemctl enable --now noise-node
journalctl -u noise-node -f
```

**先把单元里的 `--input` / `--output` 改成 `--list` 看到的设备 id 或名字。** 设备名在插拔后会变，id 更稳；换设备后要重新改。

---

## 8. 输出文件

- `events/event-<seq>-<unix_ms>.wav`：32-bit float、单声道、输入实际采样率；包含 `--pre-roll-ms` 的前置音频和完整事件。
- `events/events.jsonl`：一行一个事件，包含时间、类型、时长、冲击数、峰值、WAV 路径、是否回放、延迟和增益。

回放时会把首尾静音裁掉（保留峰值 ±200 ms），但仍播放原始波形，不做合成/变调。

**关于“延迟 3 秒”**：计时从检测器判定事件结束开始，而事件结束包含默认 1.5 s 的 hangover，再加 WAV 写入耗时。因此从物理声音到实际回放通常是 **4–5 秒**，不是精确的 3.000 秒。要缩短可以调小 `--hangover`，但会改变脚步合并行为。

---

## 9. 常见问题

| 现象 | 处理 |
| --- | --- |
| `--list` 看不到麦克风 | `arecord -l` 确认 ALSA 能看到；`sudo usermod -aG audio 用户名` 后重新登录 |
| 打开输入/输出流失败 | 设备被别的程序占用；或同一块声卡不支持同时全双工。换一块 USB 声卡/把输入输出分开 |
| 回放没有声音 | `--output` 选错（HDMI/板载 3.5 mm）；用 `aplay -l` 对照；`--gain-db` 是否太低 |
| 蓝牙音箱在 `--list` 里不存在 | cpal 走 ALSA，蓝牙 sink 通常经 PipeWire/BlueALSA；先设成系统默认 sink，或改用有线 |
| 一切都在触发 | 麦克风低频噪声/空调；先 `--min-rise 20`、`--impact-only`，再按 `detector-tuning.md` 调 |
| 拖凳子不触发 | 试 `--min-rise 10 --hangover 2500`，再试 `--no-lf-ratio`；仍不行是麦克风低频问题 |
| 回放自触发/重复触发 | 降低 `--gain-db`，把音箱挪远；蓝牙延迟用有线验证后再排查 |
| 磁盘一直涨 | 最小版本没有保留策略；自己清理 `events/` 或挂定时任务 |
| 时间戳不准 | Pi 3B 没有 RTC；联网 NTP，离线加 DS3231 |

---

## 10. 已知限制（最小版本）

- **不处理设备热插拔/断流**：USB 麦克风拔掉后需要重启 `noise-node` 或 systemd 服务。完整的退避重连在 `noise-audio` 的 `engine.rs` 里还没实现。
- **没有保留策略**：事件 WAV 会一直增长；需要自己清理或挂定时任务。
- **没有校准**：日志和 JSONL 里的电平是 dBFS，不是 dB SPL。需要绝对声压级时加声校准器，见 [acoustics-notes.md](acoustics-notes.md) 第四节。
- **没有 AEC**：用半双工门控 + 冷却期；回放期间不检测。蓝牙延迟可能破坏门控时序，先有线。
- **没有 UI/远程状态**：只有 stdout/journald 和文件。
- **不检测 31.5 Hz 以下**：默认 10 ms 帧，低频分辨率有限；见 [detector-tuning.md](detector-tuning.md) 第二节。
- **回放是自动的**：默认每个事件都会排一次回放（受门控 12 次/分钟限制）。如果不希望自动回击，先不要接输出设备，或把 `--gain-db` 调到 -80。

**法律提醒**：自动播放声音回击邻居容易升级冲突，也可能违反当地噪声、治安或租赁规定。建议先只保存证据，人工确认后再决定是否启用回放。
