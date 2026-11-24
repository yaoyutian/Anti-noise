//! Phase 3 场景测试：合成信号 → **真实 DSP 链** → 状态机。
//!
//! # 为什么不喂手搓的特征值
//!
//! 直接构造 `FrameFeatures` 来测检测器，测的是「检测器对我以为的信号
//! 的反应」。63 Hz 的周期（16 ms）比帧长（10 ms）还长，低频段的帧级
//! 直觉基本都是错的——`docs/detector-tuning.md` 第一节就是被这件事
//! 打脸的记录。所以这里每一帧特征都由 `offline::extract` 从真实样本
//! 过一遍触发带和八度带算出来，与实机走的是同一条路径。
//!
//! # 与计划的两处偏离（都是有意的）
//!
//! 1. **主判据是帧间上升沿，不是帧内峰值因数**。峰值因数在 10 ms 帧下
//!    无法区分冲击与稳态噪声，实测见 `docs/detector-tuning.md`。
//! 2. **场景 4 的期望是「零冲击」而不是「零事件」**。稳态隆隆声确实会
//!    起始一个事件，但它被归类为 `Sustained` 且 `warrants_response()`
//!    为假——不会反击。把它记成一条持续型事件是**正确**的（邻居的
//!    洗衣机同样是扰民事实），把它记成一次冲击才是错的。
//!
//! 这两条偏离都在 Phase 2 的汇报中向用户说明过。

use noise_detect::config::DetectorConfig;
use noise_detect::gate::HalfDuplexGate;
use noise_detect::offline::extract;
use noise_detect::sm::{Action, Detector};
use noise_dsp::features::FrameFeatures;
use noise_dsp::testgen;
use noise_core::EventKind;

const FS: f64 = 48_000.0;

// ---------------------------------------------------------------- 信号构造

/// 房间本底：低电平白噪声。检测器需要一个非零的底噪才能工作。
fn bed(seconds: f64, amplitude: f64, seed: u64) -> Vec<f64> {
    testgen::white_noise((FS * seconds) as usize, amplitude, seed)
}

/// 在 `x` 的 `[start_s, start_s + dur_s)` 区间**叠加**一个正弦。
///
/// 叠加而非替换：真实房间里邻居的脚步是加在环境噪声之上的，
/// 替换会得到一个不存在的静音背景。
fn overlay_tone(x: &mut [f64], start_s: f64, dur_s: f64, freq: f64, amp: f64) {
    let lo = (start_s * FS) as usize;
    let hi = ((start_s + dur_s) * FS) as usize;
    let hi = hi.min(x.len());
    if lo >= hi {
        return;
    }
    for (n, s) in x[lo..hi].iter_mut().enumerate() {
        *s += amp * (std::f64::consts::TAU * freq * n as f64 / FS).sin();
    }
}

/// 在 `[start_s, start_s + dur_s)` 区间叠加增幅调制过的白噪声——用于
/// 制造「没有单一频率」的持续低频声（空调/车流）。
fn overlay_noise(x: &mut [f64], start_s: f64, dur_s: f64, amp: f64, seed: u64) {
    let lo = (start_s * FS) as usize;
    let hi = (((start_s + dur_s) * FS) as usize).min(x.len());
    if lo >= hi {
        return;
    }
    let n = hi - lo;
    let noise = testgen::white_noise(n, 1.0, seed);
    for (i, s) in x[lo..hi].iter_mut().enumerate() {
        *s += amp * noise[i];
    }
}

// ---------------------------------------------------------------- 度量辅助

fn cfg_default() -> DetectorConfig {
    DetectorConfig::default()
}

/// 一段信号在触发带里的稳态均方值（跳过起始段）。
fn band_ms(x: &[f64]) -> f64 {
    let e = extract(x, FS, &cfg_default(), 1.0);
    let n = e.features.len();
    let mid = &e.features[n / 4..3 * n / 4];
    mid.iter().map(|f| f.trigger_mean_square).sum::<f64>() / mid.len() as f64
}

/// 求能让 63 Hz 正弦在触发带内产生 `target_ms` 均方值的幅度。
///
/// **这个换算不能靠心算。** 触发带是 20–200 Hz 的 8 阶巴特沃斯，
/// 63 Hz 落在带内但不在中心，实际通过的功率要实测才知道。用测出来的
/// 斜率反推幅度，「+25 dB」才是真的 +25 dB。
fn tone_amp_for_ms(target_ms: f64) -> f64 {
    let unit = {
        let mut x = vec![0.0; (FS * 2.0) as usize];
        overlay_tone(&mut x, 0.0, 2.0, 63.0, 1.0);
        band_ms(&x)
    };
    // 功率随幅度平方缩放，所以幅度按平方根反推。
    (target_ms / unit).sqrt()
}

/// 本底噪声在触发带里的均方值。
fn bed_ms(amplitude: f64) -> f64 {
    band_ms(&bed(4.0, amplitude, 42))
}

/// 让「本底 + 突发」的总功率高出本底 `excess_db`，所需的突发均方值。
///
/// 本底与突发不相关，功率相加：P_total = P_bed + P_burst。
/// 所以 P_burst = P_bed·(10^(excess/10) − 1)。
fn burst_amp_for_excess(bed_amplitude: f64, excess_db: f64) -> f64 {
    let p_bed = bed_ms(bed_amplitude);
    let p_burst = p_bed * (10f64.powf(excess_db / 10.0) - 1.0);
    tone_amp_for_ms(p_burst)
}

/// 一段持续低频声，已缩放到高出本底 `excess_db`（触发带内的功率超出）。
///
/// 返回的是**已经缩放好的待叠加波形**，直接加到 `bed(total_s, 0.02, ..)`
/// 上即可。
///
/// * `tilt_hz = None` —— 平坦铺满触发带（20–200 Hz）
/// * `tilt_hz = Some(fc)` —— 再叠一个 `fc` 的低通，得到**低频主导**的形状
///
/// 两者是两类不同的误报源，不能合并：空调/车流/水泵的能量集中在
/// 20–100 Hz，到 200 Hz 已经掉下去一大截，靠「升起来了但没落下」被挡成
/// 持续型；而平坦铺到 200 Hz 的低频声在 250 Hz 八度带里并不比 63 Hz 带
/// 少，连第一道门都过不去。用同一段信号测两件事，测出来的只会是
/// 「哪一段信号我碰巧调对了」。
fn sustained_low_frequency(
    total_s: f64,
    start_s: f64,
    dur_s: f64,
    excess_db: f64,
    tilt_hz: Option<f64>,
) -> Vec<f64> {
    let mut rumble = vec![0.0; (FS * total_s) as usize];
    overlay_noise(&mut rumble, start_s, dur_s, 1.0, 77);
    let mut band = noise_detect::offline::build_trigger_band(FS);
    band.process_in_place(&mut rumble);
    if let Some(fc) = tilt_hz {
        let mut tilt = noise_dsp::cascade::Cascade::new(
            noise_dsp::design::butterworth_lowpass(FS, fc, 4).sections,
        );
        tilt.process_in_place(&mut rumble);
    }

    // 功率不相关可加：P_total = P_bed + P_rumble
    let p_bed = bed_ms(0.02);
    let p_rum = band_ms(&rumble);
    let scale = (p_bed * (10f64.powf(excess_db / 10.0) - 1.0) / p_rum).sqrt();
    for s in rumble.iter_mut() {
        *s *= scale;
    }
    rumble
}

/// 整段流里最大的**帧间上升量**，算法与 `sm::Detector::deferred_rising` 一致。
///
/// 必须一致：拿另一套算法算出来的数去解释检测器的行为，只会得出
/// 「代码没按文档跑」这种查不出所以然的结论。
fn max_deferred_rise_db(feats: &[FrameFeatures]) -> f64 {
    let cfg = cfg_default();
    let w = cfg.rise_window_frames + 1;
    let mut window: Vec<f64> = Vec::new();
    let mut max_rise = f64::NEG_INFINITY;
    for f in feats {
        if window.len() >= 2 {
            let n = window.len();
            let onset = window[n - 1].max(f.trigger_db());
            let base = window[..n - 1]
                .iter()
                .cloned()
                .fold(f64::NEG_INFINITY, f64::max);
            max_rise = max_rise.max(onset - base);
        }
        window.push(f.trigger_db());
        if window.len() > w {
            window.remove(0);
        }
    }
    max_rise
}

/// 把整段信号跑过检测器，收集全部动作。
fn run(det: &mut Detector, frames: &[FrameFeatures]) -> Vec<Action> {
    let mut out = Vec::new();
    for f in frames {
        if let Some(a) = det.process(f) {
            out.push(a);
        }
    }
    out
}

fn ended(actions: &[Action]) -> Vec<&noise_detect::sm::EndedEvent> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::EventEnded(e) => Some(e),
            _ => None,
        })
        .collect()
}

fn impacts(actions: &[Action]) -> Vec<&Action> {
    actions
        .iter()
        .filter(|a| matches!(a, Action::Impact { .. }))
        .collect()
}

// ------------------------------------------------------------------ 场景 1

/// 100 ms 的 63 Hz 突发，高出底噪 +25 dB → **恰好**一个事件、一次冲击。
///
/// 「恰好」是重点：多一个说明迟滞或 hangover 没兜住，少一个说明
/// 判据太严。两端都断言。
#[test]
fn scenario_1_clear_burst_produces_exactly_one_event_and_one_impact() {
    let bed_amp = 0.02;
    let mut x = bed(6.0, bed_amp, 1);
    overlay_tone(&mut x, 3.0, 0.100, 63.0, burst_amp_for_excess(bed_amp, 25.0));

    let feats = extract(&x, FS, &cfg_default(), 1.0).features;
    let mut det = Detector::new(cfg_default());
    let actions = run(&mut det, &feats);

    let events = ended(&actions);
    assert_eq!(events.len(), 1, "应恰好一个事件，实得 {events:#?}");
    assert_eq!(events[0].impacts, 1, "应恰好一次冲击，实得 {events:#?}");
    assert_eq!(events[0].kind, EventKind::Impact);
    assert!(events[0].kind.warrants_response(), "冲击型应触发反击");
    assert_eq!(impacts(&actions).len(), 1);

    // 事件时间戳应落在突发附近，而不是流开头。
    let t = events[0].started_ms;
    assert!(
        (2000.0..=4000.0).contains(&t),
        "事件起始时刻应在突发附近（3000 ms），实得 {t} ms"
    );
}

// ------------------------------------------------------------------ 场景 2

/// 同样的突发但只高出 +8 dB（低于 15 dB 起始余量）→ **零**事件。
///
/// 这是灵敏度下限。太灵敏的系统会把正常生活声全部记成邻居噪音，
/// 那样取证记录反而失去说服力。
#[test]
fn scenario_2_burst_below_start_margin_produces_nothing() {
    let bed_amp = 0.02;
    let mut x = bed(6.0, bed_amp, 2);
    overlay_tone(&mut x, 3.0, 0.100, 63.0, burst_amp_for_excess(bed_amp, 8.0));

    let feats = extract(&x, FS, &cfg_default(), 1.0).features;
    let mut det = Detector::new(cfg_default());
    let actions = run(&mut det, &feats);

    assert!(
        actions.is_empty(),
        "+8 dB 的突发不该产生任何动作，实得 {actions:#?}"
    );
    assert_eq!(det.counters().events_started, 0);
}

// ------------------------------------------------------------------ 场景 3

/// 1 Hz 脉冲串持续 5 s → **一个**事件含 **5 次**冲击，不是 5 个事件。
///
/// 这直接决定了记录的可读性：邻居走一趟应该是一条「持续 5 秒、5 次
/// 脚步」的记录，而不是 5 条各自孤立的记录——后者不但列表被淹没，
/// 而且每次都会各自触发一次反击（把 L4 的配额白白烧掉）。
///
/// 注意这条同时钉住了 `hangover_ms` 必须 > 脉冲间隔（1000 ms）。
#[test]
fn scenario_3_pulse_train_merges_into_one_event_with_five_impacts() {
    let bed_amp = 0.02;
    let mut x = bed(10.0, bed_amp, 3);
    let amp = burst_amp_for_excess(bed_amp, 25.0);
    for k in 0..5 {
        overlay_tone(&mut x, 3.0 + k as f64 * 1.0, 0.060, 63.0, amp);
    }

    let cfg = DetectorConfig {
        cooldown_ms: 2000.0,
        ..cfg_default()
    };
    let feats = extract(&x, FS, &cfg, 1.0).features;
    let mut det = Detector::new(cfg);
    let actions = run(&mut det, &feats);

    let events = ended(&actions);
    assert_eq!(
        events.len(),
        1,
        "1 Hz 脉冲串应合并成一个事件，实得 {} 个：{events:#?}",
        events.len()
    );
    assert_eq!(
        events[0].impacts, 5,
        "应确认 5 次冲击，实得 {}",
        events[0].impacts
    );
    assert_eq!(impacts(&actions).len(), 5);
}

// ------------------------------------------------------------------ 场景 4

/// **最重要的误报测试**：60 秒的稳态低频隆隆声（空调、车流、水泵）
/// → 零冲击，且归类为持续型。
///
/// 期望不是「零事件」而是「零**冲击**」：起始一个事件是对的（持续型
/// 噪声同样是扰民事实，值得留档），把它算成脚步才是错的。区别在于
/// `warrants_response()`——持续型不反击。
///
/// 它也顺带验证了 `hangover` 与底噪冻结的配合：事件期间底噪冻结，
/// 所以这 60 秒是一条连续记录，而不是每 3.5 秒重开一条。
#[test]
fn scenario_4_steady_rumble_is_sustained_not_impact() {
    let mut x = bed(70.0, 0.02, 4);
    // +25 dB 而不是 +20：判据量的是相对**前 50 ms 里最响那一刻**的上升量，
    // 而那段窗口是对噪声取最大值（偏置约 +3.4 dB），突发第一帧又被滤波器
    // 的建立过程压低几 dB。+20 dB 的标称超出量落到判据上只剩 13 dB 左右，
    // 会在 15 dB 的门槛下随机地时过时不过——测试会变成掷骰子。
    let rumble = sustained_low_frequency(70.0, 5.0, 60.0, 25.0, Some(100.0));
    for (s, r) in x.iter_mut().zip(rumble.iter()) {
        *s += r;
    }

    let feats = extract(&x, FS, &cfg_default(), 1.0).features;

    // 前提一：起始确实够陡，事件确实开得出来。不成立的话下面那些断言
    // 会在「压根没事件」的情况下全部空转通过。
    let max_rise = max_deferred_rise_db(&feats);
    assert!(
        max_rise >= cfg_default().min_rise_db,
        "测试自身的前提：隆隆声的起始上升量（{max_rise:.1} dB）应达到判据 \
         （{:.0} dB）",
        cfg_default().min_rise_db
    );
    // 前提二：它是低频主导的，不该被 `min_lf_ratio_db` 提前挡下——
    // 那是另一条测试的事（见下一条）。用中位数而不是最大值：稳态的
    // 八度带估计起伏很大，取最大值总能捞到一个过线的瞬间。
    let steady_lf_ratio = median_lf_ratio_db(&feats, 10_000.0);
    assert!(
        steady_lf_ratio > cfg_default().min_lf_ratio_db.unwrap(),
        "测试自身的前提：稳态低频突出度中位数（{steady_lf_ratio:.1} dB）\
         应高于判据（{:.1} dB）",
        cfg_default().min_lf_ratio_db.unwrap()
    );

    let mut det = Detector::new(cfg_default());
    let actions = run(&mut det, &feats);
    let events = ended(&actions);

    assert!(
        impacts(&actions).is_empty(),
        "稳态隆隆声不该产生任何冲击（其最大帧间上升量 {max_rise:.1} dB，\
         判据 min_rise_db = {:.0} dB）：{actions:#?}",
        cfg_default().min_rise_db
    );
    for e in &events {
        assert_eq!(
            e.kind,
            EventKind::Sustained,
            "持续源必须归类为 Sustained：{e:#?}"
        );
        assert_eq!(e.impacts, 0);
        assert!(
            !e.kind.warrants_response(),
            "持续源绝不能触发反击——对着一直响的空调反击只会把自己变成新的噪声源"
        );
    }
    assert!(
        det.counters().candidates_rejected > 0,
        "应至少否决过一次候选冲击（升起来了但没落下），实得 {}",
        det.counters().candidates_rejected
    );
}

/// **第一道误报防线**：频谱平坦、一直铺到 200 Hz 的持续低频声。
///
/// 它和场景 4 是两回事，所以单独一条。场景 4 的空调声是**低频主导**的
/// （能量集中在 20–100 Hz）：电平判据和上升沿判据都放它过去，靠
/// 「升起来了但没落下」在冲击确认环节把它挡成 `Sustained`。这一条则连
/// 第一道门都过不去——`min_lf_ratio_db` 判的是 63 Hz 带减去 250–500 Hz
/// 段，而这种声音的能量一直铺到 200 Hz，250 Hz 八度带比 63 Hz 带还宽，
/// 实测低频突出度**中位数约 −0.4 dB**。
///
/// 期望是**一个事件都开不出来**，而且否决的理由必须是低频突出度——
/// 否则这段流会以别的理由被挡下，测试会在什么都没验证的情况下变绿。
#[test]
fn spectrally_flat_low_frequency_source_is_rejected_outright() {
    let mut x = bed(70.0, 0.02, 44);
    let rumble = sustained_low_frequency(70.0, 5.0, 60.0, 25.0, None);
    for (s, r) in x.iter_mut().zip(rumble.iter()) {
        *s += r;
    }

    let feats = extract(&x, FS, &cfg_default(), 1.0).features;

    // 前提一：这段流确实起得来（不然连"被挡下"的机会都没有）
    let max_rise = max_deferred_rise_db(&feats);
    assert!(
        max_rise >= cfg_default().min_rise_db,
        "测试自身的前提：起始上升量（{max_rise:.1} dB）应达到判据（{:.0} dB）\
         ——否则它会是「电平不够」而不是「频谱不合」被挡下的",
        cfg_default().min_rise_db
    );
    // 前提二：它的低频突出度确实低于判据
    let steady_lf_ratio = median_lf_ratio_db(&feats, 10_000.0);
    assert!(
        steady_lf_ratio < cfg_default().min_lf_ratio_db.unwrap(),
        "测试自身的前提：稳态低频突出度中位数（{steady_lf_ratio:.1} dB）\
         应低于判据（{:.1} dB）",
        cfg_default().min_lf_ratio_db.unwrap()
    );

    let mut det = Detector::new(cfg_default());
    let actions = run(&mut det, &feats);

    assert_eq!(
        det.counters().events_started,
        0,
        "低频突出度不达标的持续声不该开出事件，实得动作 {actions:#?}"
    );
    assert!(impacts(&actions).is_empty());
    assert!(
        det.counters().suppressed_by_lf_ratio > 0,
        "应留下「因低频突出度不足被挡下」的痕迹——否则用户漏检时无从下手"
    );
    // 而且必须是**边沿触发**：每帧都记一次会让计数器变成帧数，没有信息量。
    assert_eq!(
        det.counters().suppressed_by_lf_ratio, 1,
        "同一次突发只该记一次抑制"
    );
}

/// `t_ms` 之后的低频突出度中位数。
///
/// 取中位数而不是最大值或最小值：稳态的八度带估计起伏很大（实测这一
/// 段信号的 63 Hz 带在 24 dB 的范围内摆动），任何极值统计量都会被
/// 单个瞬间主导，只有中位数反映这段信号**大体上**是什么形状。
fn median_lf_ratio_db(feats: &[FrameFeatures], after_ms: f64) -> f64 {
    let mut v: Vec<f64> = feats
        .iter()
        .filter(|f| f.t_ms > after_ms)
        .map(|f| f.lf_ratio_db())
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

// ------------------------------------------------------------------ 场景 5

/// 环境整体 +10 dB 阶跃 → 底噪跟上去，且跟踪期间零事件。
///
/// 这是「白天开空调之后，夜里那套阈值就失效了」的回归测试。+10 dB
/// 刻意选在起始余量（15 dB）之下：它该被底噪吸收，而不是被当成事件。
#[test]
fn scenario_5_environment_step_is_absorbed_by_the_floor() {
    let bed_amp = 0.02;
    let mut x = bed(60.0, bed_amp, 5);
    // 从第 10 秒起整段抬高 10 dB
    let step = 10f64.powf(10.0 / 20.0); // 幅度倍数 = 10^(dB/20)
    for s in x[(10.0 * FS) as usize..].iter_mut() {
        *s *= step;
    }

    let mut cfg = cfg_default();
    cfg.floor_warmup_s = 0.0; // 本测试要看的是稳态跟踪，不是启动预热
    let feats = extract(&x, FS, &cfg, 1.0).features;

    let mut det = Detector::new(cfg);
    let actions = run(&mut det, &feats);

    assert!(
        actions.is_empty(),
        "环境阶跃不该产生任何事件，实得 {actions:#?}"
    );

    // 底噪应从阶跃前的水平大致跟上 10 dB。
    let floor_final = det.floor_db(1.0);
    let floor_early = {
        // 重跑一遍，只喂前 8 秒，取那时的底噪
        let mut d2 = Detector::new(cfg);
        for f in &feats[..800] {
            d2.process(f);
        }
        d2.floor_db(1.0)
    };
    let tracked = floor_final - floor_early;
    assert!(
        tracked > 6.0,
        "底噪应至少跟上 6 dB（升时间常数 30 s，60 s 内应基本到位），\
         实测只移动了 {tracked:.1} dB"
    );
    assert!(
        tracked < 12.0,
        "底噪不该越过环境本身（+10 dB），实测移动了 {tracked:.1} dB"
    );
}

// ------------------------------------------------------------------ 场景 6

/// `Cooldown` 期间的突发 → 被抑制，且计数器递增。
///
/// 冷却期的存在意义是「刚反击完先安静一会儿」。但它必须是**可见**的：
/// 「什么都没发生」和「发生了但被挡下」对调参的含义完全相反，
/// 只记前者会让用户在漏检时无从下手。
#[test]
fn scenario_6_burst_during_cooldown_is_suppressed_and_counted() {
    let bed_amp = 0.02;
    let mut x = bed(8.0, bed_amp, 6);
    overlay_tone(&mut x, 4.0, 0.100, 63.0, burst_amp_for_excess(bed_amp, 25.0));

    let cfg = cfg_default();
    let feats = extract(&x, FS, &cfg, 1.0).features;

    let gate = HalfDuplexGate::new(cfg);
    let mut det = Detector::with_gate(cfg, gate);

    // 在突发之前手动占住门控：请求一次回放并立刻结束它 → 进入冷却期。
    let t_burst = 4000.0;
    {
        let g = det.gate_mut().expect("门控应存在");
        g.request_response(3500.0).expect("首次回放应被允许");
        g.finish_response(3900.0, 400.0);
        assert_eq!(g.state(), noise_detect::gate::GateState::Cooldown);
    }

    let actions = run(&mut det, &feats);

    let suppressed: Vec<&Action> = actions
        .iter()
        .filter(|a| matches!(a, Action::Suppressed { .. }))
        .collect();
    assert_eq!(
        suppressed.len(),
        1,
        "冷却期应恰好抑制一次（边沿触发，不是每帧一次），实得 {suppressed:#?}"
    );
    assert!(matches!(
        suppressed[0],
        Action::Suppressed {
            reason: noise_detect::sm::Suppression::CoolingDown,
            ..
        }
    ));
    assert_eq!(det.counters().suppressed_by_gate, 1);
    assert_eq!(det.counters().events_started, 0, "冷却期不该开事件");
    assert!(
        feats.iter().any(|f| f.t_ms >= t_burst && f.t_ms < t_burst + 200.0),
        "测试自身的前提：突发确实落在了冷却期内"
    );
}

// ------------------------------------------------------------------ 场景 7

/// **关闭回放时，检测路径可证明完全一致（L0）。**
///
/// 同一段特征流跑两遍：一遍用 `Detector::new`（回放关闭，门控根本
/// 不存在），一遍用 `Detector::with_gate`（门控存在但从未回放过）。
/// 两遍的动作序列必须**逐字节相同**。
///
/// 这是「关掉回放就没有任何副作用」这句话的可执行形式。它比代码
/// 评审强的地方在于：将来谁往检测路径里加一句「顺便看一眼门控状态」，
/// 这个测试立刻变红。
#[test]
fn scenario_7_gate_absent_when_playback_disabled() {
    let bed_amp = 0.02;
    let mut x = bed(12.0, bed_amp, 7);
    let amp = burst_amp_for_excess(bed_amp, 25.0);
    for k in 0..3 {
        overlay_tone(&mut x, 3.0 + k as f64 * 2.0, 0.080, 63.0, amp);
    }
    overlay_tone(&mut x, 9.0, 0.100, 100.0, amp * 0.8);

    let cfg = cfg_default();
    let feats = extract(&x, FS, &cfg, 1.0).features;

    let mut without_gate = Detector::new(cfg);
    let a = run(&mut without_gate, &feats);

    let mut with_gate = Detector::with_gate(cfg, HalfDuplexGate::new(cfg));
    let b = run(&mut with_gate, &feats);

    assert_eq!(
        a.len(),
        b.len(),
        "两种构建的动作数量不同：{} vs {}",
        a.len(),
        b.len()
    );
    assert_eq!(a, b, "关闭回放时门控的存在绝不能改变任何一个动作");
    assert!(
        !a.is_empty(),
        "测试自身的前提：这段流应当确实产生了动作"
    );

    // 也断言阈值推导没有收耦合校正——没有耦合观测就不该有校正。
    for action in &a {
        if let Action::EventStarted { thresholds, .. } = action {
            assert!(
                !thresholds.coupling_corrected,
                "从未回放过就不该声称做过耦合校正"
            );
        }
    }
}

// ------------------------------------------------------------------ 场景 8

/// `Muted`（正在回放）期间的突发 → 不开事件、不计数抑制，但
/// `observe_self_noise` 记录下自己回放造成的电平（L3 的数据来源）。
///
/// **这里与计划有一处措辞上的收紧。** 计划写的是「被抑制，
/// `suppressed_by_gate` 递增」，但回放期间的抑制不该算进门控抑制——
/// 那个计数器是给用户看「有多少次真实噪声因为门控被漏掉了」的，
/// 把自己回放的每一个瞬态都算进去会让它在正常使用中永远不为零，
/// 变成一个没有信息量的数字。所以回放期间只观测、不记抑制。
#[test]
fn scenario_8_burst_while_muted_is_observed_not_detected() {
    let bed_amp = 0.02;
    let mut x = bed(8.0, bed_amp, 8);
    overlay_tone(&mut x, 4.0, 0.100, 63.0, burst_amp_for_excess(bed_amp, 25.0));

    let cfg = cfg_default();
    let feats = extract(&x, FS, &cfg, 1.0).features;

    let gate = HalfDuplexGate::new(cfg);
    let mut det = Detector::with_gate(cfg, gate);

    // 从第 3.5 秒起进入回放并一直保持到结束。
    det.gate_mut()
        .expect("门控应存在")
        .request_response(3500.0)
        .expect("首次回放应被允许");

    let actions = run(&mut det, &feats);

    assert_eq!(det.counters().events_started, 0, "回放期间不该开事件");
    assert!(
        !actions.iter().any(|a| matches!(a, Action::EventStarted { .. })),
        "回放期间不该有任何事件：{actions:#?}"
    );
    assert_eq!(
        det.counters().suppressed_by_gate,
        0,
        "回放期间的抑制不该计入「门控挡掉了多少真实噪声」"
    );
    assert!(
        det.counters().frames_while_muted > 0,
        "应记录到处于静音期的帧数"
    );

    // 耦合观测：突发（自己的回放）确实被观测到了，且被记为高出底噪。
    let coupling = det
        .gate()
        .expect("门控应存在")
        .coupling_excess_db()
        .expect("回放期间应记录到耦合");
    assert!(
        coupling > 10.0,
        "自己放出来的 +25 dB 突发应被记为显著耦合，实得 {coupling:.1} dB"
    );
}

// ------------------------------------------------------- 门控与检测的交互

/// `Muted` 期间**必须**把进行中的事件结束掉。
///
/// 理由：回放期间听不到楼上，事件的后续既无从判断也不该被编造。
/// 留着它不收尾，会在回放结束后把两段互不相关的录音拼成一条记录。
#[test]
fn entering_mute_ends_the_active_event() {
    let bed_amp = 0.02;
    let mut x = bed(8.0, bed_amp, 9);
    overlay_tone(&mut x, 3.0, 0.100, 63.0, burst_amp_for_excess(bed_amp, 25.0));

    let cfg = cfg_default();
    let feats = extract(&x, FS, &cfg, 1.0).features;

    let gate = HalfDuplexGate::new(cfg);
    let mut det = Detector::with_gate(cfg, gate);

    let mut started_at: Option<f64> = None;
    let mut ended_while_muted = false;

    for f in &feats {
        // 事件一起来就开始回放
        if det.active_event().is_some() && started_at.is_none() {
            started_at = Some(f.t_ms);
            let g = det.gate_mut().unwrap();
            g.request_response(f.t_ms).expect("回放应被允许");
        }
        if let Some(Action::EventEnded(e)) = det.process(f) {
            if let Some(t0) = started_at {
                assert!(
                    f.t_ms >= t0,
                    "事件结束时序错乱：{t0} → {}",
                    f.t_ms
                );
                ended_while_muted = true;
                assert!(
                    e.duration_ms < 1000.0,
                    "进入回放应立即结束事件，而不是拖到 hangover 之后\
                     （实得 {:.0} ms）",
                    e.duration_ms
                );
            }
        }
    }

    assert!(started_at.is_some(), "测试自身的前提：应确实起始过事件");
    assert!(ended_while_muted, "进入回放时进行中的事件必须被结束");
    assert_eq!(det.active_event(), None, "回放结束后不该还有挂着的事件");
}

// ------------------------------------------------------------- 不变量性质

/// 任何有限输入都不该让检测器产生 NaN / 无穷，也不该在没起始事件时
/// 就结束事件。
///
/// 这条覆盖的是最容易出事的一类输入：全零、恒定直流、极大值、
/// 逐帧交替的极端电平。真实设备在拔插或切换采样率时会短暂给出
/// 这类数据。
#[test]
fn degenerate_inputs_never_produce_nan_or_orphan_events() {
    let cases: Vec<Vec<f64>> = vec![
        vec![0.0; 48_000],
        vec![0.5; 48_000],
        vec![1.0; 48_000],
        (0..48_000)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect(),
        (0..48_000)
            .map(|i| if (i / 480) % 2 == 0 { 1e-9 } else { 0.9 })
            .collect(),
    ];

    for (i, x) in cases.iter().enumerate() {
        let feats = extract(x, FS, &cfg_default(), 1.0).features;
        let mut det = Detector::new(cfg_default());
        let mut depth: i32 = 0;
        for f in &feats {
            let Some(a) = det.process(f) else { continue };
            match a {
                Action::EventStarted { .. } => depth += 1,
                Action::EventEnded(_) => {
                    depth -= 1;
                    assert!(
                        depth >= 0,
                        "输入 {i}：出现了没有起始事件的 EventEnded"
                    );
                }
                _ => {}
            }
            if let Action::EventEnded(e) = a {
                assert!(
                    e.started_ms.is_finite()
                        && e.ended_ms.is_finite()
                        && e.duration_ms.is_finite()
                        && e.peak_db.is_finite()
                        && e.end_db.is_finite(),
                    "输入 {i}：事件含非有限字段 {e:#?}"
                );
                assert!(e.duration_ms >= 0.0, "输入 {i}：事件时长为负 {}", e.duration_ms);
            }
        }
        assert!(depth <= 1, "输入 {i}：同时有多个进行中的事件");
        assert!(det.floor_db(1.0).is_finite(), "输入 {i}：底噪变成非有限值");
    }
}

/// `force_end` 必须能收尾——停止监测、拔设备、改配置都要用它。
#[test]
fn force_end_closes_the_event_without_a_hangover() {
    let bed_amp = 0.02;
    let mut x = bed(6.0, bed_amp, 10);
    overlay_tone(&mut x, 3.0, 0.100, 63.0, burst_amp_for_excess(bed_amp, 25.0));

    let feats = extract(&x, FS, &cfg_default(), 1.0).features;
    let mut det = Detector::new(cfg_default());

    // 只喂到突发刚过，此时还在 hangover 里
    let upto = feats.iter().position(|f| f.t_ms > 3200.0).unwrap();
    for f in &feats[..upto] {
        det.process(f);
    }
    assert!(det.active_event().is_some(), "测试自身的前提：事件应还在进行中");

    let action = det.force_end(feats[upto].t_ms);
    assert!(
        matches!(action, Some(Action::EventEnded(_))),
        "force_end 应产出一个结束动作，实得 {action:?}"
    );
    assert_eq!(det.active_event(), None);

    // 已经结束了，再调一次不该产出第二个。
    assert!(det.force_end(feats[upto].t_ms + 100.0).is_none());
}
