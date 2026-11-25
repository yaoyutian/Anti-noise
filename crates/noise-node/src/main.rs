//! 树莓派最小节点：麦克风 → 检测 → 录 WAV → 延迟后原样回放。
//!
//! 这不是完整的桌面应用，而是一个可以 `cargo run -p noise-node` 起来的
//! 无头程序，用来先把「麦克风—检测—录音—喇叭」这条物理链路跑通。
//!
//! # 它做什么
//!
//! ```text
//! 输入设备（单声道）
//!   → 20–200 Hz 触发带 + 八度带 + 帧特征
//!   → noise-detect 状态机（与 replay 完全同一条检测路径）
//!   → 事件结束时从 pre-roll 环形历史里取音频
//!   → 写 events/event-*.wav，追加 events/events.jsonl
//!   → 请求半双工门控，延迟 --delay-ms（默认 3000）后把同一段音频
//!     去掉首尾静音，从输出设备原样播放
//! ```
//!
//! # 和桌面版的差别
//!
//! * 没有 UI、没有 Tauri、没有存储层的分段/保留策略。
//! * 录音是「一个事件一个 WAV」，不是连续的会话分段。
//! * 回放的是**录到的声音本身**，不是 `response.rs` 里的 55 Hz 合成闷响。
//! * 用 `Detector::with_gate` 做半双工：回放期间和冷却期不检测，底噪冻结，
//!   并保留 L4 速率看门狗（默认 12 次/分钟）。
//!
//! # 树莓派上最重要的两件事
//!
//! 1. 麦克风必须能录到 20–200 Hz。廉价「声音传感器」模块只有阈值/包络，
//!    接不进这条链。先按 `docs/manual-test.md` 第 0 节做低频自检。
//! 2. 蓝牙音箱的 A2DP 延迟通常是上百毫秒，而门控按「回放时长」计时。
//!    第一次实机测试请优先用 USB/有线音箱；蓝牙要单独测延迟。
//!
//! 详细硬件与部署说明见 `docs/raspberry-pi-minimal.md`。
//!
//! # 法律提醒
//!
//! 自动回放会升级冲突，也可能违反当地噪声/治安规定。建议第一阶段只
//! 保存证据、关闭回放（把 `--delay-ms` 设得很大或先不接输出设备），
//! 确认检测质量后再决定是否启用。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{bounded, Sender, TryRecvError, TrySendError};
use noise_audio::device::{find_input, find_output};
use noise_audio::{
    build_input_stream, build_output_stream, enumerate, AudioConsumer, AudioHistory, DeviceCatalog,
    InputBuild, MAX_GAIN_DB, MIN_GAIN_DB,
};
use noise_core::{DetectorConfig, EventKind};
use noise_detect::{Action, Detector, HalfDuplexGate};
use noise_dsp::design::{butterworth_highpass, butterworth_lowpass};
use noise_dsp::features::{spectral_flatness_db, FrameFeatures};
use noise_dsp::metrics::{crest_factor_db, mean_square, peak_abs};
use noise_dsp::{Cascade, OctaveBank};

/// 触发带下限，Hz。与 `noise-detect` 的离线路径保持一致。
const TRIGGER_LOW_HZ: f64 = 20.0;
/// 触发带上限，Hz。
const TRIGGER_HIGH_HZ: f64 = 200.0;
/// 触发带每侧阶数。
const TRIGGER_ORDER: usize = 4;
/// 输入/输出环容量，秒。
const RING_SECONDS: f64 = 2.0;
/// 历史缓冲在 pre-roll + 最长事件之外多留的余量，秒。
const HISTORY_SLACK_S: f64 = 5.0;
/// 回放前裁剪首尾静音的门限，相对峰值的 dB。
const TRIM_REL_DB: f32 = -40.0;
/// 裁剪时在第一个/最后一个有效样本外保留的余量，毫秒。
const TRIM_PAD_MS: f64 = 200.0;
/// 门控 `finish_response` 相对回放结束的额外余量，毫秒。
const GATE_FINISH_MARGIN_MS: f64 = 200.0;

fn main() {
    if let Err(e) = run() {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = parse_args(&std::env::args().skip(1).collect::<Vec<_>>())?;
    if args.help {
        print_help();
        return Ok(());
    }
    if args.list {
        list_devices()?;
        return Ok(());
    }

    let catalog = enumerate().map_err(|e| format!("枚举音频设备失败：{e}"))?;
    let input_id = resolve_device(&catalog, &args.input, true)?;
    let output_id = resolve_device(&catalog, &args.output, false)?;

    let input_device = find_input(&input_id).map_err(|e| format!("找不到输入设备：{e}"))?;
    let mut input = build_input_stream(
        &input_device,
        Some(args.sample_rate),
        (args.sample_rate as f64 * RING_SECONDS) as usize,
    )
    .map_err(|e| format!("打开输入流失败：{e}"))?;
    input.play()?;

    let input_rate = input.info.sample_rate;
    let input_summary = input.info.summary();
    let input_notes = input.notes.clone();

    let InputBuild {
        stream: input_stream,
        consumer: input_consumer,
        info: _,
        notes: _,
        errors: input_errors,
        error_counters: _,
    } = input;
    // Stream 必须由建它的线程持有；这里只要它活到 run() 结束。
    let _input_stream = input_stream;

    let output_device = find_output(&output_id).map_err(|e| format!("找不到输出设备：{e}"))?;
    // 优先让输出也用输入的实际采样率，避免重采样；设备不支持时协商会退回。
    let mut output = build_output_stream(
        &output_device,
        Some(input_rate),
        (input_rate as f64 * RING_SECONDS) as usize,
    )
    .map_err(|e| format!("打开输出流失败：{e}"))?;
    output.play()?;

    let output_rate = output.info.sample_rate;
    let output_summary = output.info.summary();
    let gain_db = args.gain_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
    // 让分析线程写进 events.jsonl 的 gain_db 与实际生效值一致。
    args.gain_db = gain_db;

    fs::create_dir_all(&args.frames_dir)
        .map_err(|e| format!("创建事件目录 {} 失败：{e}", args.frames_dir.display()))?;
    let jsonl_path = args.frames_dir.join("events.jsonl");
    let jsonl = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&jsonl_path)
        .map_err(|e| format!("打开 {} 失败：{e}", jsonl_path.display()))?;

    let history_secs = args.pre_roll_ms / 1000.0 + args.max_event_ms / 1000.0 + HISTORY_SLACK_S;
    let (tx, rx) = bounded::<PlaybackRequest>(4);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let thread_args = args.clone();

    let analysis = thread::Builder::new()
        .name("noise-analysis".to_string())
        .spawn(move || {
            if let Err(e) = analysis_loop(
                input_consumer,
                input_rate,
                output_rate,
                tx,
                thread_args,
                history_secs,
                jsonl,
                stop_thread,
            ) {
                eprintln!("[分析] 退出：{e}");
            }
        })
        .map_err(|e| format!("启动分析线程失败：{e}"))?;

    println!("输入    {input_summary}");
    for n in &input_notes {
        println!("  注意  {n}");
    }
    println!("输出    {output_summary}");
    if output_rate != input_rate {
        println!("  注意  输入 {input_rate} Hz / 输出 {output_rate} Hz，回放会做线性重采样");
    }
    println!(
        "回放    延迟 {:.0} ms，增益 {gain_db:.1} dB，事件目录 {}",
        args.delay_ms,
        args.frames_dir.display()
    );
    println!(
        "检测    上升 ≥ {:.1} dB，起始底噪 +{:.1} dB，hangover {:.0} ms，cooldown {:.0} ms{}",
        args.cfg.min_rise_db,
        args.cfg.start_margin_db,
        args.cfg.hangover_ms,
        args.cfg.cooldown_ms,
        if args.cfg.min_lf_ratio_db.is_none() {
            "，低频占比判据关闭"
        } else {
            ""
        }
    );
    println!("就绪。Ctrl+C 退出。\n");

    let mut pending_request: Option<PlaybackRequest> = None;
    let mut active: Option<ActivePlayback> = None;
    let started = Instant::now();

    loop {
        if analysis.is_finished() {
            eprintln!("[主] 分析线程已退出");
            break;
        }

        while let Ok(e) = input_errors.try_recv() {
            eprintln!("[输入] {e}");
        }
        while let Ok(e) = output.errors.try_recv() {
            eprintln!("[输出] {e}");
        }

        if active.is_none() && pending_request.is_none() {
            match rx.try_recv() {
                Ok(req) => pending_request = Some(req),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if active.is_none() {
            if let Some(req) = pending_request.take() {
                if Instant::now() < req.at {
                    pending_request = Some(req);
                } else if output.control.is_idle() {
                    if let Some(_plan) = output.control.begin(gain_db, req.samples.len() as u64) {
                        println!(
                            "[回放] 事件 #{} 开始（{} 样本）",
                            req.seq,
                            req.samples.len()
                        );
                        active = Some(ActivePlayback {
                            samples: req.samples,
                            offset: 0,
                            seq: req.seq,
                        });
                    } else {
                        println!("[回放] 输出忙，丢弃事件 #{}", req.seq);
                    }
                } else {
                    pending_request = Some(req);
                }
            }
        }

        let mut finished_seq = None;
        if let Some(a) = active.as_mut() {
            let free = output.producer.free_slots();
            if free > 0 && a.offset < a.samples.len() {
                let end = (a.offset + free).min(a.samples.len());
                let dropped = output.producer.push_partial(&a.samples[a.offset..end]);
                a.offset = end - dropped;
                if dropped > 0 {
                    eprintln!("[回放] 输出环满，丢弃 {dropped} 样本");
                }
            }

            if a.offset >= a.samples.len() {
                let done = output.control.is_complete()
                    || (output.control.is_idle() && !output.control.is_playing());
                if done {
                    finished_seq = Some(a.seq);
                }
            }
        }
        if let Some(seq) = finished_seq {
            println!("[回放] 事件 #{seq} 结束");
            active = None;
        }

        if let Some(limit) = args.run_for_s {
            if started.elapsed() >= Duration::from_secs_f64(limit) {
                println!("[主] 到达 --run-for，退出");
                break;
            }
        }

        thread::sleep(Duration::from_millis(2));
    }

    stop.store(true, Ordering::Relaxed);
    if analysis.join().is_err() {
        eprintln!("[主] 分析线程异常退出");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Args {
    help: bool,
    list: bool,
    input: String,
    output: String,
    delay_ms: f64,
    pre_roll_ms: f64,
    max_event_ms: f64,
    gain_db: f64,
    sample_rate: u32,
    frames_dir: PathBuf,
    impact_only: bool,
    run_for_s: Option<f64>,
    cfg: DetectorConfig,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            help: false,
            list: false,
            input: String::new(),
            output: String::new(),
            delay_ms: 3000.0,
            pre_roll_ms: 2000.0,
            max_event_ms: 12_000.0,
            gain_db: 0.0,
            sample_rate: 48_000,
            frames_dir: PathBuf::from("events"),
            impact_only: false,
            run_for_s: None,
            cfg: DetectorConfig::default(),
        }
    }
}

fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut i = 0usize;

    while i < raw.len() {
        let arg = raw[i].as_str();
        let value = |i: &mut usize, what: &str| -> Result<String, String> {
            *i += 1;
            raw.get(*i)
                .cloned()
                .ok_or_else(|| format!("{what} 后面少了值"))
        };

        match arg {
            "-h" | "--help" => a.help = true,
            "-l" | "--list" => a.list = true,
            "--input" | "--input-device" => a.input = value(&mut i, arg)?,
            "--output" | "--output-device" => a.output = value(&mut i, arg)?,
            "--delay-ms" | "--delay" => a.delay_ms = num(&value(&mut i, arg)?, arg)?,
            "--pre-roll-ms" | "--pre-roll" => a.pre_roll_ms = num(&value(&mut i, arg)?, arg)?,
            "--max-event-ms" => a.max_event_ms = num(&value(&mut i, arg)?, arg)?,
            "--gain-db" => a.gain_db = num(&value(&mut i, arg)?, arg)?,
            "--sample-rate" => {
                a.sample_rate = value(&mut i, arg)?
                    .parse()
                    .map_err(|_| "--sample-rate 要一个整数 Hz".to_string())?
            }
            "--frames-dir" => a.frames_dir = PathBuf::from(value(&mut i, arg)?),
            "--impact-only" => a.impact_only = true,
            "--run-for" => a.run_for_s = Some(num(&value(&mut i, arg)?, arg)?),
            "--min-rise" => a.cfg.min_rise_db = num(&value(&mut i, arg)?, arg)?,
            "--start-margin" => a.cfg.start_margin_db = num(&value(&mut i, arg)?, arg)?,
            "--stop-margin" => a.cfg.stop_margin_db = num(&value(&mut i, arg)?, arg)?,
            "--hangover" => a.cfg.hangover_ms = num(&value(&mut i, arg)?, arg)?,
            "--cooldown" => a.cfg.cooldown_ms = num(&value(&mut i, arg)?, arg)?,
            "--decay" => a.cfg.min_impact_decay_db = num(&value(&mut i, arg)?, arg)?,
            "--frame-ms" => a.cfg.frame_ms = num(&value(&mut i, arg)?, arg)?,
            "--lf-ratio" => a.cfg.min_lf_ratio_db = Some(num(&value(&mut i, arg)?, arg)?),
            "--no-lf-ratio" => a.cfg.min_lf_ratio_db = None,
            other if other.starts_with('-') => {
                return Err(format!("不认识的参数 `{other}`（--help 看用法）"))
            }
            other => return Err(format!("多余的位置参数 `{other}`")),
        }
        i += 1;
    }

    if !a.delay_ms.is_finite() || a.delay_ms < 0.0 {
        return Err("--delay-ms 必须是非负数".into());
    }
    if !a.pre_roll_ms.is_finite() || a.pre_roll_ms < 0.0 || a.pre_roll_ms > 60_000.0 {
        return Err("--pre-roll-ms 必须在 0–60000 ms 之间".into());
    }
    if !a.max_event_ms.is_finite() || a.max_event_ms < 50.0 || a.max_event_ms > 120_000.0 {
        return Err("--max-event-ms 必须在 50–120000 ms 之间".into());
    }
    if !a.gain_db.is_finite() {
        return Err("--gain-db 必须是有限数".into());
    }
    if let Some(x) = a.cfg.min_lf_ratio_db {
        if !x.is_finite() {
            return Err("--lf-ratio 必须是有限数".into());
        }
    }
    if a.sample_rate < 16_000 {
        return Err("--sample-rate 不能低于 16000 Hz".into());
    }
    if let Some(s) = a.run_for_s {
        if !s.is_finite() || s <= 0.0 {
            return Err("--run-for 必须是正数秒".into());
        }
    }
    a.cfg
        .validate()
        .map_err(|e| format!("检测参数不合法：{e}"))?;
    Ok(a)
}

fn num(s: &str, flag: &str) -> Result<f64, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("{flag} 要一个数，给了 `{s}`"))
}

fn print_help() {
    println!(
        "\
用法: noise-node [选项]

设备:
  -l, --list                 列出输入/输出设备后退出
  --input <id或名字>         输入设备（默认：系统默认）
  --output <id或名字>        输出设备（默认：系统默认）

回放:
  --delay-ms <ms>            录制结束后延迟多久回放（默认 3000）
  --gain-db <dB>             回放增益（默认 0；上限 0，负值衰减）
  --impact-only              只有 Impact 事件回放；Sustained 只保存
  --frames-dir <dir>         事件 WAV 和 events.jsonl 的目录（默认 events）

录音:
  --pre-roll-ms <ms>         事件开始前额外保留的音频（默认 2000）
  --max-event-ms <ms>        超过这个时长的事件不录不回放（默认 12000；
                             用来避免把空调/水泵的长事件当成回击素材）
  --sample-rate <Hz>         输入首选采样率（默认 48000，实际以协商为准）

检测参数（默认值见 crates/noise-core/src/detector_config.rs）:
  --min-rise <dB>            起始所需最小上升量（默认 15）
  --start-margin <dB>        起始门限高出底噪（默认 15）
  --stop-margin <dB>         停止门限高出底噪（默认 6）
  --hangover <ms>            事件结束后保持时间（默认 1500）
  --cooldown <ms>            事件结束后的抑制期（默认 2000）
  --decay <dB>               确认冲击所需的衰减量（默认 10）
  --frame-ms <ms>            分析帧长（默认 10；不要随意改）
  --lf-ratio <dB>            低频优势下限（默认 6）
  --no-lf-ratio              关闭低频优势判据（麦克风低频差时排查用）

其他:
  --run-for <秒>             运行指定时间后退出（自检用）
  -h, --help                 显示本帮助

例子:
  noise-node --list
  noise-node --input \"USB Audio\" --output \"USB Audio\" --delay-ms 3000
  noise-node --input \"USB Audio\" --output \"USB Audio\" --no-lf-ratio --min-rise 10
"
    );
}

// ---------------------------------------------------------------------------
// 设备
// ---------------------------------------------------------------------------

fn list_devices() -> Result<(), String> {
    let cat = enumerate().map_err(|e| format!("枚举音频设备失败：{e}"))?;
    println!("宿主: {}", cat.host);
    println!("输入设备:");
    if cat.inputs.is_empty() {
        println!("  （无）");
    }
    for d in &cat.inputs {
        print_device(d);
    }
    println!("输出设备:");
    if cat.outputs.is_empty() {
        println!("  （无）");
    }
    for d in &cat.outputs {
        print_device(d);
    }
    Ok(())
}

fn print_device(d: &noise_audio::DeviceInfo) {
    println!("  {}{}", d.name, if d.is_default { "  [默认]" } else { "" });
    println!("    id: {}", d.id);
    if let Some(f) = &d.default_format {
        println!(
            "    默认: {}–{} Hz / {} 声道 / {} / {} bit",
            f.min_sample_rate, f.max_sample_rate, f.channels, f.format, f.bits
        );
    }
    for f in &d.formats {
        println!(
            "    可选: {}–{} Hz / {} ch / {} / {} bit",
            f.min_sample_rate, f.max_sample_rate, f.channels, f.format, f.bits
        );
    }
    if let Some(e) = &d.error {
        println!("    错误: {e}");
    }
}

fn resolve_device(cat: &DeviceCatalog, want: &str, is_input: bool) -> Result<String, String> {
    if want.is_empty() {
        return Ok(String::new());
    }
    let list = if is_input { &cat.inputs } else { &cat.outputs };
    if list.iter().any(|d| d.id == want) {
        return Ok(want.to_string());
    }
    if let Some(d) = list.iter().find(|d| d.name.eq_ignore_ascii_case(want)) {
        return Ok(d.id.clone());
    }
    if let Some(d) = list.iter().find(|d| d.name.contains(want)) {
        return Ok(d.id.clone());
    }
    Err(format!(
        "找不到{}设备 `{want}`；用 --list 查看可用的 id/名字",
        if is_input { "输入" } else { "输出" }
    ))
}

// ---------------------------------------------------------------------------
// 分析线程
// ---------------------------------------------------------------------------

struct ActiveEvent {
    started_ms: f64,
}

struct ActivePlayback {
    samples: Vec<f32>,
    offset: usize,
    seq: u64,
}

struct PlaybackRequest {
    at: Instant,
    samples: Vec<f32>,
    seq: u64,
}

#[allow(clippy::too_many_arguments)]
fn analysis_loop(
    mut consumer: AudioConsumer,
    input_rate: u32,
    output_rate: u32,
    tx: Sender<PlaybackRequest>,
    args: Args,
    history_secs: f64,
    mut jsonl: File,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let fs = input_rate as f64;
    let cfg = args.cfg;
    let frame_len = ((fs * cfg.frame_ms / 1000.0).round() as usize).max(1);
    let frame_ms = frame_len as f64 * 1000.0 / fs;

    let mut trigger = build_trigger(fs);
    let mut octave = OctaveBank::with_default_bands(fs);
    let mut detector = Detector::with_gate(cfg, HalfDuplexGate::new(cfg));
    let mut history = AudioHistory::with_seconds(fs, history_secs);

    let mut pending = Vec::<f32>::with_capacity(frame_len * 4);
    let pop_len = ((fs * 0.05).round() as usize).max(1);
    let mut pop = vec![0.0f32; pop_len];
    let mut frame_f32 = vec![0.0f32; frame_len];
    let mut frame_f64 = vec![0.0f64; frame_len];

    let mut frame_index = 0u64;
    let mut active_event: Option<ActiveEvent> = None;
    let mut pending_finish: Option<(Instant, f64)> = None;
    let mut played_count = 0u64;

    println!(
        "[分析] 监听中：帧长 {frame_ms:.2} ms，历史 {history_secs:.1} s，底噪预热约 {:.0} s",
        cfg.floor_warmup_s
    );

    while !stop.load(Ordering::Relaxed) {
        let n = consumer.pop_into(&mut pop);
        if n == 0 {
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        history.push(&pop[..n]);
        pending.extend_from_slice(&pop[..n]);

        while pending.len() >= frame_len {
            frame_f32.copy_from_slice(&pending[..frame_len]);
            pending.drain(..frame_len);

            let frame_t = frame_index as f64 * frame_ms;

            // 门控计时用检测器的帧时间轴，不用墙钟；这里在帧边界上收尾
            // 一次回放，下一帧 detector.process 会看到 Cooldown。
            if let Some((at, response_ms)) = pending_finish {
                if Instant::now() >= at {
                    if let Some(gate) = detector.gate_mut() {
                        gate.finish_response(frame_t, response_ms);
                    }
                    pending_finish = None;
                    println!("[门控] 回放结束，进入冷却期");
                }
            }

            for (dst, src) in frame_f64.iter_mut().zip(frame_f32.iter()) {
                *dst = *src as f64;
            }

            let broadband_ms = mean_square(&frame_f64);
            let mut filtered = frame_f64.clone();
            trigger.process_in_place(&mut filtered);
            let trigger_ms = mean_square(&filtered);
            let crest_db = crest_factor_db(peak_abs(&filtered), trigger_ms.sqrt());

            let mut bands_db = vec![0.0f64; octave.num_bands()];
            octave.process_db(&frame_f64, &mut bands_db, 1.0);
            let flatness = spectral_flatness_db(&bands_db);

            let features = FrameFeatures {
                frame_index,
                t_ms: frame_t,
                trigger_mean_square: trigger_ms,
                broadband_mean_square: broadband_ms,
                crest_db,
                bands_db,
                flatness,
                reference: 1.0,
            };

            if let Some(action) = detector.process(&features) {
                match action {
                    Action::EventStarted {
                        seq,
                        t_ms,
                        trigger_db,
                        floor_db,
                        thresholds,
                    } => {
                        active_event = Some(ActiveEvent { started_ms: t_ms });
                        println!(
                            "[{:8.2}s] 事件开始 #{seq}  触发 {trigger_db:7.1} dBFS  底噪 {floor_db:7.1}  起始门限 {:7.1}",
                            t_ms / 1000.0,
                            thresholds.start_db
                        );
                    }
                    Action::Impact {
                        seq,
                        t_ms,
                        onset_db,
                        peak_db,
                        decay_db,
                    } => {
                        println!(
                            "[{:8.2}s]   └ 冲击 #{seq}  起音 {onset_db:7.1}  峰值 {peak_db:7.1}  衰减 {decay_db:5.1} dB",
                            t_ms / 1000.0
                        );
                    }
                    Action::EventEnded(e) => {
                        let start_ms = active_event
                            .take()
                            .map(|a| a.started_ms)
                            .unwrap_or(e.started_ms);
                        println!(
                            "[{:8.2}s] 事件结束 #{}  类型 {:?}  时长 {:.0} ms  冲击 {}  峰值 {:.1} dBFS",
                            e.ended_ms / 1000.0,
                            e.seq,
                            e.kind,
                            e.duration_ms,
                            e.impacts,
                            e.peak_db
                        );

                        if e.duration_ms > args.max_event_ms {
                            println!(
                                "  跳过：事件 {:.0} ms 超过 --max-event-ms {:.0} ms，\
                                 更像持续源（空调/水泵），不录不回放",
                                e.duration_ms, args.max_event_ms
                            );
                        } else {
                            let start_sample = ms_to_samples(start_ms, fs);
                            let end_sample = ms_to_samples(e.ended_ms, fs);
                            let pre = ms_to_samples(args.pre_roll_ms, fs);
                            let from = start_sample.saturating_sub(pre);
                            let oldest = history.oldest_index();
                            if from < oldest {
                                println!(
                                    "  注意：历史缓冲不足，录音从 {:.2}s 开始（请求 {:.2}s）",
                                    oldest as f64 / fs,
                                    from as f64 / fs
                                );
                            }

                            let mut full = Vec::new();
                            history.copy_range(from, end_sample, &mut full);
                            if full.len() < (fs * 0.05) as usize {
                                println!("  跳过：录音短于 50 ms");
                            } else {
                                let wall_ms = unix_ms();
                                let wav_path = args
                                    .frames_dir
                                    .join(format!("event-{:04}-{wall_ms}.wav", e.seq));
                                let mut saved = true;
                                if let Err(err) = write_wav(&wav_path, input_rate, &full) {
                                    saved = false;
                                    eprintln!("  录音保存失败：{err}");
                                }

                                // 回放前去掉首尾静音，但 WAV 保留完整 pre-roll。
                                let clip = trim_clip(&full, fs);
                                let mut played = false;

                                if args.impact_only && e.kind != EventKind::Impact {
                                    println!("  --impact-only：{:?} 只保存不回放", e.kind);
                                } else if clip.is_empty() {
                                    println!("  跳过回放：录音里没有超过 -40 dB 相对峰值的样本");
                                } else {
                                    let out = resample_linear(&clip, input_rate, output_rate);
                                    if out.is_empty() {
                                        println!("  跳过回放：重采样后为空");
                                    } else {
                                        let clip_ms =
                                            out.len() as f64 * 1000.0 / output_rate as f64;
                                        let gate_ok = if let Some(gate) = detector.gate_mut() {
                                            match gate.request_response(e.ended_ms) {
                                                Ok(()) => true,
                                                Err(reason) => {
                                                    println!("  回放被门控拒绝：{reason:?}");
                                                    false
                                                }
                                            }
                                        } else {
                                            false
                                        };

                                        if gate_ok {
                                            let at = Instant::now()
                                                + Duration::from_secs_f64(args.delay_ms / 1000.0);
                                            match tx.try_send(PlaybackRequest {
                                                at,
                                                samples: out,
                                                seq: e.seq,
                                            }) {
                                                Ok(()) => {
                                                    played = true;
                                                    played_count += 1;
                                                    pending_finish = Some((
                                                        at + Duration::from_secs_f64(
                                                            (clip_ms + GATE_FINISH_MARGIN_MS)
                                                                / 1000.0,
                                                        ),
                                                        clip_ms,
                                                    ));
                                                    println!(
                                                        "  已排定 {:.1}s 后回放（{:.2}s，第 {} 次）",
                                                        args.delay_ms / 1000.0,
                                                        clip_ms / 1000.0,
                                                        played_count
                                                    );
                                                }
                                                Err(TrySendError::Full(_)) => {
                                                    if let Some(gate) = detector.gate_mut() {
                                                        gate.finish_response(e.ended_ms, 0.0);
                                                    }
                                                    println!("  回放队列满，取消本次回放");
                                                }
                                                Err(TrySendError::Disconnected(_)) => {
                                                    if let Some(gate) = detector.gate_mut() {
                                                        gate.finish_response(e.ended_ms, 0.0);
                                                    }
                                                    println!("  回放接收端已断开，取消本次回放");
                                                }
                                            }
                                        }
                                    }
                                }

                                let entry = serde_json::json!({
                                    "wall_time_ms": wall_ms as u64,
                                    "seq": e.seq,
                                    "kind": format!("{:?}", e.kind),
                                    "started_ms": e.started_ms,
                                    "ended_ms": e.ended_ms,
                                    "duration_ms": e.duration_ms,
                                    "impacts": e.impacts,
                                    "rejected_candidates": e.rejected_candidates,
                                    "peak_db": e.peak_db,
                                    "wav": wav_path.display().to_string(),
                                    "wav_saved": saved,
                                    "played": played,
                                    "delay_ms": args.delay_ms,
                                    "gain_db": args.gain_db,
                                });
                                let line = serde_json::to_string(&entry)
                                    .unwrap_or_else(|_| "{}".to_string());
                                let write_result = writeln!(jsonl, "{line}");
                                let write_result = write_result.and_then(|_| jsonl.flush());
                                if let Err(err) = write_result {
                                    eprintln!("  events.jsonl 写入失败：{err}");
                                }
                            }
                        }
                    }
                    Action::Suppressed {
                        reason,
                        t_ms,
                        trigger_db,
                    } => {
                        println!(
                            "[{:8.2}s] 已抑制（{reason:?}）  触发 {trigger_db:7.1} dBFS",
                            t_ms / 1000.0
                        );
                    }
                }
            }

            frame_index += 1;
        }
    }

    Ok(())
}

fn build_trigger(fs: f64) -> Cascade {
    let hp = butterworth_highpass(fs, TRIGGER_LOW_HZ, TRIGGER_ORDER);
    let lp = butterworth_lowpass(fs, TRIGGER_HIGH_HZ, TRIGGER_ORDER);
    if hp.degraded || lp.degraded {
        eprintln!("[分析] 警告：采样率过低，触发带设计频率被钳位");
    }
    let mut sections = hp.sections;
    sections.extend(lp.sections);
    Cascade::new(sections)
}

fn ms_to_samples(ms: f64, fs: f64) -> u64 {
    ((ms / 1000.0) * fs).round().max(0.0) as u64
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 去掉首尾静音，保留峰值 ±200 ms。
///
/// 只裁剪静音，不改波形；用于回放，WAV 里保留完整 pre-roll。
fn trim_clip(x: &[f32], fs: f64) -> Vec<f32> {
    if x.is_empty() {
        return Vec::new();
    }
    let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak < 1e-7 {
        return Vec::new();
    }
    let threshold = peak * 10f32.powf(TRIM_REL_DB / 20.0);
    let first = x.iter().position(|v| v.abs() >= threshold).unwrap_or(0);
    let last = x
        .iter()
        .rposition(|v| v.abs() >= threshold)
        .unwrap_or(x.len() - 1);
    let pad = ((fs * TRIM_PAD_MS / 1000.0).round() as usize).max(1);
    let start = first.saturating_sub(pad);
    let end = (last + pad + 1).min(x.len());
    x[start..end].to_vec()
}

/// 线性插值重采样。只用于输入/输出采样率不一致的兜底，不做抗混叠。
fn resample_linear(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let n = ((x.len() as f64) * ratio).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let src = i as f64 / ratio;
        let i0 = src.floor() as usize;
        let frac = src - i0 as f64;
        let a = x.get(i0).copied().unwrap_or(0.0);
        let b = x.get(i0 + 1).copied().unwrap_or(a);
        out.push((a as f64 * (1.0 - frac) + b as f64 * frac) as f32);
    }
    out
}

fn write_wav(path: &Path, sample_rate: u32, samples: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("创建 {} 失败：{e}", path.display()))?;
    for &s in samples {
        writer
            .write_sample(s)
            .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("收尾 {} 失败：{e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_keeps_signal_with_padding() {
        let fs = 48_000.0;
        let mut x = vec![0.0f32; 48_000];
        // 约 420 ms 处放 10 ms 的 0.5 幅度信号，其余是静音。
        for s in &mut x[20_000..20_480] {
            *s = 0.5;
        }
        let t = trim_clip(&x, fs);
        let pad = (fs * TRIM_PAD_MS / 1000.0) as usize;
        assert_eq!(t.len(), 480 + 2 * pad);
        assert_eq!(t[0], 0.0);
        assert_eq!(t[pad + 240], 0.5);
    }

    #[test]
    fn trim_rejects_digital_silence() {
        assert!(trim_clip(&[0.0f32; 100], 48_000.0).is_empty());
    }

    #[test]
    fn resample_halves_the_length() {
        let x: Vec<f32> = (0..100).map(|i| i as f32).collect();
        assert_eq!(resample_linear(&x, 48_000, 24_000).len(), 50);
        assert_eq!(resample_linear(&x, 48_000, 48_000), x);
    }
}
