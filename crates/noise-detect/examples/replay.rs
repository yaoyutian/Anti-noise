//! 把一段 WAV 喂过**真正的**提取路径和检测状态机，打印 action 流。
//!
//! ```text
//! cargo run -p noise-detect --example replay -- neighbor.wav
//! cargo run -p noise-detect --example replay -- neighbor.wav --min-rise 12 --hangover 2500
//! cargo run -p noise-detect --example replay -- neighbor.wav --cal-dbfs-at-94 -28.0
//! ```
//!
//! # 为什么它必须在 Phase 1 之前就存在
//!
//! 在音频硬件就绪之前，这是**唯一**能回答「检测器在真实信号上表现如何」
//! 的东西。没有它，调参只能靠合成信号——那调出来的是对**我以为的**信号
//! 的反应。而 `docs/detector-tuning.md` 里每一个被推翻的直觉，都是在真实
//! 信号的频谱上被打脸的。
//!
//! # 它走的不是一条「测试用」的路径
//!
//! [`noise_detect::offline::extract`] 是场景测试、这个 example、以及
//! `src-tauri --replay` 共用的同一份实现，滤波器状态跨帧连续，与实时
//! 路径逐帧等价。**用录音调出来的参数在实机上成立**——这是整个工作流的
//! 前提，所以这里绝不能有一份「简化版」的提取。
//!
//! # 时基
//!
//! 默认尽快跑完（几十秒的录音一秒出结果）。`--speed 1` 按真实时钟播，
//! 用来对着现场同步观察。文件时间戳全部相对文件开头，不是墙上时间。

use std::process::ExitCode;

use noise_detect::config::DetectorConfig;
use noise_detect::offline::{extract, Extracted};
use noise_detect::sm::{Action, Detector, Suppression};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// 参数字典。手工解析——引 clap 只为这一个 example 不值得。
struct Args {
    path: String,
    speed: f64,
    cal_dbfs_at_94: Option<f64>,
    cfg: DetectorConfig,
    /// 只打印摘要，不打每一帧的门限变化。
    quiet: bool,
}

fn run(raw: &[String]) -> Result<(), String> {
    let a = parse_args(raw)?;

    let (samples, fs, channels) = read_wav_mono(&a.path)?;
    let duration_s = samples.len() as f64 / fs;

    println!("文件      {}", a.path);
    println!(
        "采样率    {fs} Hz{}",
        if fs < 16_000.0 {
            "   ← 低于 16 kHz，滤波器会被钳位"
        } else {
            ""
        }
    );
    println!(
        "声道      {channels}（已降混单声道）",
    );
    println!("时长      {duration_s:.2} s / {} 样本", samples.len());

    // 参考值决定 dB 的**零点**，不改变任何判据（判据全是差值）。
    // 没有校准就把 1.0 当 0 dBFS；有校准则换算成 dB SPL 直读。
    let reference = match a.cal_dbfs_at_94 {
        Some(cal) => {
            println!("校准      94 dB SPL = {cal:.2} dBFS → 下面全部读作 dB SPL");
            10f64.powf((cal - 94.0) / 20.0)
        }
        None => {
            println!("校准      无 → 下面全部读作 dBFS（**不是**声压级）");
            1.0
        }
    };

    let started = std::time::Instant::now();
    let e = extract(&samples, fs, &a.cfg, reference);
    if e.degraded {
        println!("⚠ DSP 降级  触发带或八度带的设计频率被钳位（采样率过低）");
    }
    println!(
        "提取      {} 帧 × {:.2} ms（实际帧长 {} 样本，尾部丢弃 {}）",
        e.features.len(),
        e.frame_ms,
        e.frame_len,
        e.tail_samples
    );
    if e.features.is_empty() {
        return Err(format!(
            "文件短于一个分析帧（{} 样本 < {}），没有可判定的内容",
            samples.len(),
            e.frame_len
        ));
    }
    println!();

    let actions = feed(&e, &a.cfg, a.speed, a.quiet);
    let elapsed = started.elapsed();

    println!();
    report(&e, &actions, &a.cfg);
    println!();
    println!(
        "耗时      {:.2} s（{:.2}× 实时）",
        elapsed.as_secs_f64(),
        duration_s / elapsed.as_secs_f64().max(1e-9)
    );
    Ok(())
}

fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut path: Option<String> = None;
    let mut speed = 0.0f64; // 0 = 尽快
    let mut cal = None;
    let mut quiet = false;
    let mut cfg = DetectorConfig::default();

    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].as_str();
        // 取值参数统一走这里，省得每个分支各写一遍「少了值」的报错。
        let value = |i: &mut usize, what: &str| -> Result<String, String> {
            *i += 1;
            raw.get(*i)
                .cloned()
                .ok_or_else(|| format!("{what} 后面少了值"))
        };
        match arg {
            "--speed" => speed = value(&mut i, "--speed")?.parse().map_err(|_| "--speed 要一个数".to_string())?,
            "--cal-dbfs-at-94" => {
                cal = Some(value(&mut i, "--cal-dbfs-at-94")?.parse().map_err(|_| "--cal-dbfs-at-94 要一个数".to_string())?)
            }
            "--quiet" => quiet = true,
            "--min-rise" => cfg.min_rise_db = num(&value(&mut i, arg)?, arg)?,
            "--start-margin" => cfg.start_margin_db = num(&value(&mut i, arg)?, arg)?,
            "--stop-margin" => cfg.stop_margin_db = num(&value(&mut i, arg)?, arg)?,
            "--hangover" => cfg.hangover_ms = num(&value(&mut i, arg)?, arg)?,
            "--cooldown" => cfg.cooldown_ms = num(&value(&mut i, arg)?, arg)?,
            "--decay" => cfg.min_impact_decay_db = num(&value(&mut i, arg)?, arg)?,
            "--lf-ratio" => {
                cfg.min_lf_ratio_db = Some(num(&value(&mut i, arg)?, arg)?);
            }
            "--no-lf-ratio" => cfg.min_lf_ratio_db = None,
            "--crest" => cfg.min_crest_db = Some(num(&value(&mut i, arg)?, arg)?),
            "--frame-ms" => cfg.frame_ms = num(&value(&mut i, arg)?, arg)?,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other if other.starts_with('-') => return Err(format!("不认识的参数 `{other}`（--help 看用法）")),
            other => {
                if path.is_some() {
                    return Err(format!("给了不止一个文件：已经要了 {path:?}，又来了 {other:?}"));
                }
                path = Some(other.to_string());
            }
        }
        i += 1;
    }

    let path = path.ok_or_else(|| {
        print_help();
        "缺少 WAV 路径".to_string()
    })?;

    cfg.validate().map_err(|e| format!("参数不合法：{e}"))?;

    Ok(Args {
        path,
        speed,
        cal_dbfs_at_94: cal,
        cfg,
        quiet,
    })
}

fn num(s: &str, flag: &str) -> Result<f64, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("{flag} 要一个数，给了 `{s}`"))
}

fn print_help() {
    eprintln!(
        "\
用法: replay <wav> [选项]

  --speed <N>            0（默认）= 尽快跑完；1 = 按真实时钟
  --cal-dbfs-at-94 <dB>  94 dB SPL 对应的 dBFS 读数，给了就按 dB SPL 显示
  --quiet                只打事件，不打每帧的门限变化

覆盖默认参数（都不给就是 config.rs 里的默认值）：
  --min-rise <dB>        起始所需上升量        默认 15
  --start-margin <dB>    起始门限高于底噪      默认 15
  --stop-margin <dB>     停止门限高于底噪      默认 6
  --hangover <ms>        低于停止门限后的保持  默认 1500
  --cooldown <ms>        抑制冷却期            默认 2000
  --decay <dB>           确认冲击所需的衰减量  默认 10
  --lf-ratio <dB>        低频优势下限          默认 6
  --no-lf-ratio          关掉低频优势判据
  --crest <dB>           帧内峰均比下限（默认关）
  --frame-ms <ms>        分析帧长              默认 10"
    );
}

/// 按帧喂检测器，边喂边打。返回全部 action。
///
/// 打印与喂食写在同一个循环里，是因为 action 的**时刻**就是它的内容：
/// 先收集再统一打印会丢掉「第 3010 帧」和「第 3010 帧的 30.1 s」之间
/// 那个对应关系，而那正是调参时要看的东西。
fn feed(e: &Extracted, cfg: &DetectorConfig, speed: f64, quiet: bool) -> Vec<Action> {
    let mut det = Detector::new(*cfg);
    let mut actions = Vec::new();
    let realtime = speed > 0.0;
    let started = std::time::Instant::now();
    let mut last_thresholds = None;

    for f in &e.features {
        if let Some(a) = det.process(f) {
            if !quiet || !matches!(a, Action::Suppressed { .. }) {
                println!("{}", describe(&a));
            }
            actions.push(a);
        } else if !quiet && realtime {
            // 只在按真实时钟跑的时候打门限：那是「对着现场看」的模式，
            // 需要看到门限在爬。尽快跑完时打这个等于刷屏。
            let t = det.thresholds();
            if last_thresholds != Some(t) {
                println!(
                    "{:>9.2}s          底噪 {:>7.2}  起始 {:>7.2}  停止 {:>7.2}",
                    f.t_ms / 1000.0,
                    t.floor_db,
                    t.start_db,
                    t.stop_db
                );
                last_thresholds = Some(t);
            }
        }

        if realtime {
            let target = std::time::Duration::from_secs_f64(f.t_ms / 1000.0 / speed);
            let now = started.elapsed();
            if target > now {
                std::thread::sleep(target - now);
            }
        }
    }
    actions
}

fn describe(a: &Action) -> String {
    match a {
        Action::EventStarted {
            seq,
            t_ms,
            trigger_db,
            floor_db,
            thresholds,
        } => format!(
            "{:>9.2}s  事件开始 #{seq}   触发 {:>7.2}  底噪 {:>7.2}  超起始门限 {:>+6.2} dB",
            t_ms / 1000.0,
            trigger_db,
            floor_db,
            trigger_db - thresholds.start_db
        ),
        Action::Impact {
            seq,
            t_ms,
            onset_db,
            peak_db,
            decay_db,
        } => format!(
            "{:>9.2}s    └ 冲击   #{seq}   起音 {:>7.2}  峰值 {:>7.2}  衰减 {:>5.2} dB",
            t_ms / 1000.0,
            onset_db,
            peak_db,
            decay_db
        ),
        Action::EventEnded(e) => format!(
            "{:>9.2}s  事件结束 #{:<4}  时长 {:>7.0} ms  冲击 {}  否决 {}  {:?}  峰值 {:>7.2}",
            e.ended_ms / 1000.0,
            e.seq,
            e.duration_ms,
            e.impacts,
            e.rejected_candidates,
            e.kind,
            e.peak_db
        ),
        Action::Suppressed {
            reason,
            t_ms,
            trigger_db,
        } => format!(
            "{:>9.2}s  已抑制（{}）  触发 {:>7.2}",
            t_ms / 1000.0,
            match reason {
                Suppression::CoolingDown => "冷却期",
                Suppression::LowFrequencyRatio => "低频占比不足",
                Suppression::LowCrest => "峰值因数不足",
            },
            trigger_db
        ),
    }
}

/// 收尾统计。**这是调参时真正要看的那一屏**——单个事件的细节会骗人，
/// 「一整段录音里有几次事件、几次被低频判据挡下」不会。
fn report(e: &Extracted, actions: &[Action], cfg: &DetectorConfig) {
    let started = actions.iter().filter(|a| matches!(a, Action::EventStarted { .. })).count();
    let impacts = actions.iter().filter(|a| matches!(a, Action::Impact { .. })).count();
    let ended: Vec<&noise_detect::EndedEvent> = actions
        .iter()
        .filter_map(|a| match a {
            Action::EventEnded(e) => Some(e),
            _ => None,
        })
        .collect();

    // 末尾那个 `+ 0.0` 不是笔误：`f64` 的 `Sum` 从 `-0.0` 起步（加法的
    // 单位元），所以一段没有事件的录音会显示成「-0.0 s」。
    let total_ms: f64 = ended.iter().map(|e| e.duration_ms).sum::<f64>() + 0.0;
    let file_ms = e.features.last().map_or(0.0, |f| f.t_ms + e.frame_ms);
    let impact_events = ended.iter().filter(|e| e.impacts > 0).count();
    let rejected: u32 = ended.iter().map(|e| e.rejected_candidates).sum();

    let levels: Vec<f64> = e.features.iter().map(|f| f.trigger_db()).collect();
    let lo = levels.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = levels.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let mean = levels.iter().sum::<f64>() / levels.len() as f64;

    println!("── 汇总 ─────────────────────────────────");
    println!("触发带电平  最低 {lo:.2}  最高 {hi:.2}  平均 {mean:.2}  （动态范围 {:.1} dB）", hi - lo);
    println!("起始判定    {started} 次");
    println!("确认冲击    {impacts} 次（分布在 {impact_events} 个事件里）");
    println!("否决候选    {rejected} 次   ← 被当成持续源（空调/水泵）挡下的");
    if file_ms > 0.0 {
        println!(
            "事件占比    {:.1}%（{:.1} s / {:.1} s）",
            100.0 * total_ms / file_ms,
            total_ms / 1000.0,
            file_ms / 1000.0
        );
    }
    println!(
        "判据        上升 ≥ {:.0} dB  起始底噪+{:.0}  停止底噪+{:.0}  衰减 ≥ {:.0} dB",
        cfg.min_rise_db, cfg.start_margin_db, cfg.stop_margin_db, cfg.min_impact_decay_db
    );
    if started > 0 && impacts == 0 {
        println!();
        println!("提示：起了事件但一次冲击都没确认。若录音里确实有脚步，试 `--decay` 调小");
        println!("      或 `--lf-ratio 0`——低频占比判据在窄带麦克风上可能过不去。");
    }
    if started == 0 {
        println!();
        println!("提示：一次事件都没起。先看上面触发带电平的动态范围，再拿它跟");
        println!("      `--min-rise {:.0}` 比——范围还没判据大，就说明麦克风压根没", cfg.min_rise_db);
        println!("      录到那个频段，调检测器是没用的（见 docs/detector-tuning.md 风险 1）。");
    }
}

/// 读 WAV 并降混成单声道 f64。
///
/// 降混用**算术平均**而不是求和：求和会让双声道文件的电平凭空高 6 dB，
/// 于是同一个声音在两支麦克风上给出不同的绝对值。
///
/// 不做重采样。检测器的所有判据都是差值，滤波器按文件自己的采样率现场
/// 设计——44.1 kHz 的录音和 48 kHz 的录音因此给出实质相同的判定。引入
/// 重采样器只会多一条可能出错的路径。
fn read_wav_mono(path: &str) -> Result<(Vec<f64>, f64, u16), String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("打不开 `{path}`：{e}"))?;
    let spec = reader.spec();
    let fs = spec.sample_rate as f64;
    if spec.channels == 0 {
        return Err("WAV 声道数为 0".to_string());
    }
    if fs <= 0.0 {
        return Err("WAV 采样率为 0".to_string());
    }

    let ch = spec.channels as usize;
    let interleaved: Vec<f64> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.map(|v| v as f64))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("读 WAV 失败：{e}"))?,
        hound::SampleFormat::Int => {
            // 按位深归一化到 ±1，与 f32 文件同一个尺度。
            let full = (1i64 << (spec.bits_per_sample - 1)) as f64;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f64 / full))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("读 WAV 失败：{e}"))?
        }
    };

    let mono = if ch == 1 {
        interleaved
    } else {
        interleaved
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f64>() / ch as f64)
            .collect()
    };

    if mono.is_empty() {
        return Err("WAV 里没有样本".to_string());
    }
    Ok((mono, fs, spec.channels))
}
