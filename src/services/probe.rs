//! 不依赖 ffprobe，直接用 ffmpeg.exe 读媒体元数据。
//!
//! 程序随包分发 `ffmpeg.exe` 和 `ffprobe.exe` 两个近 100 MB 的二进制，而 ffprobe 只
//! 承担"读元数据"这一件事。这个模块用三个原语把同样的信息从 ffmpeg 里取出来：
//!
//! * [`read_banner`]：`ffmpeg -i <file>` 打到 stderr 上的横幅，拿时长、宽高、帧率、
//!   音频编码。
//! * [`exact_frame_rate`]：showinfo 滤镜初始化时打的 `frame_rate: 24000/1001`。
//!   横幅里的帧率只有两位小数（23.976 会印成 23.98），这个才是精确值。
//! * [`build_frame_table`]：`-c copy -f framecrc` 打出的逐包时间戳，只拆包不解码。
//!   可变帧率（VFR）就是靠它算帧号↔时间。
//!
//! 这个模块**不引用 `crate::` 里的任何东西**（ffmpeg 路径当参数传进来），这样
//! `examples/probe_compare.rs` 能用 `#[path]` 把同一个源文件 include 进去做对照，
//! 对照程序验证的就是主逻辑将来要用的那份代码。
//!
//! ## 几个实测出来的坑，改这个文件前先看一遍
//!
//! * **`ffmpeg -i` 成功时退出码是 1**（"At least one output file must be specified"）。
//!   判成功只能看内容：横幅里有 `Input #0` 就是打开了。
//! * **`showinfo` 的 `checksum` 默认是 `true`**，一定要显式关掉。全片扫描时
//!   实测 9.9 秒 → 4.5 秒。
//! * **`-fps_mode passthrough` 对 showinfo 没有任何影响**——它挂在输出同步的上游。
//!   而且它是输出选项，放到 `-i` 前面 ffmpeg 会当场报错。
//! * **framecrc 的数据行是解码顺序**，有 B 帧时 pts 不单调（实测 mp4 的 pts 顺序是
//!   `0, 4004, 2002, 1001, 3003`）。要显示顺序必须按 pts 排序。
//! * **framecrc 的第 4 列（包时长）不能用**。mkv/flv 是 1 毫秒时间基，一帧 41.7 ms
//!   存成 41 个刻度，反推必然得 24.39（真值 23.976）；AVI 存不下 B 帧的 PTS，
//!   那一列恒为 1，反推得 600。帧率一律走 [`exact_frame_rate`]。
//! * **不加 `-c copy` 的 `-f framecrc` 完全不能用**——它输出的是 ffmpeg 重新生成的
//!   输出时间线，不是源的。实测一个真 VFR 文件会被它报成 30fps 再掉到 15fps。

// 第 2 步把 ffmpeg.rs 接过来之前没人调用，先压住 dead_code 警告。
#![allow(dead_code)]

use regex::Regex;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 起子进程时不弹控制台窗口（和 ffmpeg.rs 里同一个值）。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 判 VFR 的相对差阈值。实测 CFR 文件最大 9.85e-05（mkv/flv 的 1 毫秒时间基地板），
/// 真 VFR 文件 3.7e-01，中间空了三个数量级，取 1e-3 很安全。
const VFR_TOLERANCE: f64 = 1e-3;

/// 等子进程退出时的轮询间隔。
const POLL: Duration = Duration::from_millis(50);

/// 每读到这么多包往主线程报一次进度。两小时 60fps 是 43 万个包，
/// 逐个报会让通道白忙。
const PROGRESS_EVERY: usize = 256;

/// 帧率、时间基这类有理数。
///
/// **不转成 f64 存**：帧号换算 `frame / fps` 拿它做精确运算，转成 f64 之后
/// 23.976 那类值就再也回不来了。
///
/// `Default` 是 `0/0`，也就是"无效"（见 [`Rational::is_valid`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

impl Rational {
    pub fn new(num: u32, den: u32) -> Self {
        Self { num, den }
    }

    /// 分母为 0 或分子为 0 都算无效（ffmpeg 对"未知帧率"就打 `0/0`）。
    pub fn is_valid(&self) -> bool {
        self.num > 0 && self.den > 0
    }

    pub fn as_f64(&self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f64 / self.den as f64
        }
    }
}

/// `ffmpeg -i` 横幅里能拿到的东西。
///
/// 打开失败时 `opened` 为 false，其余全是默认值——调用方据此区分"打不开"和
/// "打开了但某个字段没有"。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Banner {
    /// 横幅里出现了 `Input #0`，即 ffmpeg 确实把文件打开了
    pub opened: bool,
    /// 容器声明的总时长（秒）。`Duration: N/A` 或没印出来时为 0
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    /// 第一条音频流的编码名（`aac` / `pcm_s16le` / ...）；没有音轨时为 None
    pub audio_codec: Option<String>,
    /// 视频流那行的 `23.98 fps`，即 `avg_frame_rate`。只有两位小数
    pub avg_fps: f64,
    /// 视频流那行的 `23.98 tbr`，即 `r_frame_rate`。只有两位小数
    ///
    /// 两个都要：它们不相等就说明帧率在变，是 VFR 的免费一级筛子
    /// （见 [`banner_suspects_vfr`]）。
    pub r_fps: f64,
}

impl Banner {
    /// 取一个帧率用：优先 `avg_frame_rate`，没有就退回 `r_frame_rate`。
    ///
    /// 这**不是**拿来做帧号换算的那个值——两位小数的精度不够（23.976 会变成 23.98，
    /// 两小时的视频上差 29 帧）。真正的帧率走 [`exact_frame_rate`]。
    pub fn fps(&self) -> f64 {
        if self.avg_fps > 0.0 {
            self.avg_fps
        } else {
            self.r_fps
        }
    }
}

/// `-c copy -f framecrc` 打出来的逐包时间戳表。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameTable {
    pub timebase: Rational,
    /// 显示顺序、升序。单位是 `timebase` 的刻度
    pub pts: Vec<i64>,
}

impl FrameTable {
    pub fn len(&self) -> usize {
        self.pts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    /// 首帧到末帧之间的秒数。
    pub fn span_secs(&self) -> f64 {
        match (self.pts.first(), self.pts.last()) {
            (Some(a), Some(b)) => (*b - *a) as f64 * self.timebase.as_f64(),
            _ => 0.0,
        }
    }

    /// 第 `frame` 帧相对首帧的时间（秒）。越界返回 0。
    pub fn frame_to_secs(&self, frame: usize) -> f64 {
        match (self.pts.first(), self.pts.get(frame)) {
            (Some(first), Some(p)) => (*p - *first) as f64 * self.timebase.as_f64(),
            _ => 0.0,
        }
    }

    /// 某个时刻正在显示的是第几帧。
    ///
    /// 取的是**当前正在显示的那一帧**而不是"最近的那一帧"：3.4567s 时返回帧 103
    /// （它占 [3.4333, 3.4667)），不是 104。和 `trim.rs::secs_to_frame` 同一个语义。
    /// 超出末尾时返回最后一帧。
    pub fn secs_to_frame(&self, secs: f64) -> i64 {
        if self.pts.is_empty() || !secs.is_finite() || secs < 0.0 {
            return 0;
        }
        let first = self.pts[0];
        let tb = self.timebase.as_f64();
        if tb <= 0.0 {
            return 0;
        }
        // 秒换算回 timebase 的刻度。末尾那个余量见 floor_eps：调用方给的时间可能只到
        // 毫秒，而且 1001/24000 除回去本身也可能是 1000.9999999999999。
        //
        // 用 floor 而不是 round：要的是"这一刻正在显示的那一帧"，也就是时刻落在哪个
        // 区间里。round 会把某一帧区间最后半个刻度上的时刻算成下一帧（和 CFR 那支的
        // 语义对不上，CFR 用的就是 floor）。
        // saturating_add：秒数大得离谱时 `as i64` 已经饱和到 i64::MAX，
        // 再加首帧的偏移会溢出（debug 直接 panic）。
        let target = first.saturating_add(((secs / tb) + floor_eps(1.0 / tb)).floor() as i64);
        // 要的是"最后一个 pts <= target"，也就是"第一个 > target"再退一格
        self.pts.partition_point(|&p| p <= target).saturating_sub(1) as i64
    }

    /// 缓存计费用。`pts` 是唯一的堆分配。
    pub fn approx_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.pts.capacity() * std::mem::size_of::<i64>()
    }
}

// ---- 帧号换算 ----

/// `floor` 之前加的那点余量，单位跟被 floor 的那个数一致：CFR 那支是帧，帧表
/// 那支是 timebase 刻度。`ticks_per_sec` 就是"一秒有多少个这种单位"，CFR 传帧率，
/// 帧表传 1/时间基。
///
/// 为什么要留余量：传进来的时间常常只精确到毫秒。用户在时间框里手输的时间码就写到
/// 毫秒，真实时间被就近舍入过，最多能比真值小 0.5 毫秒。逐帧停在帧边界上时真值是个
/// 整数刻度，舍完就掉到整数下面，`floor` 落到上一帧——用户看到的就是"按一下下一帧，
/// 帧号不动"。24fps 下 16000 的时间基上是 8 个刻度，每三帧撞一次。
///
/// 取半毫秒不多不少：再小补不回那次舍入，再大就会把边界下方本来属于上一帧的报数
/// 也推上去。边界两侧 0.5 毫秒内的报数本来就分不清（同一个毫秒值可能来自任意一边），
/// 这里一律按"算下一帧"处理，宁可多算也不让逐帧掉格子。
///
/// 下限 1e-6 是给帧率低得离谱的文件兜底。
fn floor_eps(ticks_per_sec: f64) -> f64 {
    (ticks_per_sec * 0.0005).max(1e-6)
}

/// 帧号和时间戳怎么互相换。
///
/// 恒定帧率只需要一个有理数，一个表都不存；可变帧率必须逐帧查表。两者对调用方
/// 是同一套接口，所以不用在哪里写 `if vfr`。
#[derive(Debug, Clone, PartialEq)]
pub enum Framing {
    /// 恒定帧率。线性换算，实测在那个 10466 帧的真实文件上误差 0.01 帧
    Cfr(Rational),
    /// 可变帧率。`fps` 只是流声明的名义帧率，实际换算一律走表
    Vfr { fps: Rational, table: FrameTable },
    /// 帧率拿不到（没有视频轨，或者读不出来）
    Unknown,
}

impl Framing {
    pub fn is_valid(&self) -> bool {
        match self {
            Framing::Cfr(r) => r.is_valid(),
            Framing::Vfr { .. } => true,
            Framing::Unknown => false,
        }
    }

    /// 名义帧率，给日志和"有没有帧率"这类判断用。换算别用它。
    pub fn fps(&self) -> Rational {
        match self {
            Framing::Cfr(r) => *r,
            Framing::Vfr { fps, .. } => *fps,
            Framing::Unknown => Rational::default(),
        }
    }

    pub fn fps_f64(&self) -> f64 {
        self.fps().as_f64()
    }

    /// 源帧号换算成秒（第 0 帧 = 源视频第一帧）。
    ///
    /// 负数帧号、或者帧率拿不到时返回 0，校验留给调用方。
    pub fn frame_to_secs(&self, frame: i64) -> f64 {
        if frame < 0 {
            return 0.0;
        }
        match self {
            Framing::Cfr(r) if r.is_valid() => frame as f64 * r.den as f64 / r.num as f64,
            Framing::Vfr { table, .. } => table.frame_to_secs(frame as usize),
            _ => 0.0,
        }
    }

    /// 秒换算成源帧号，是 [`Framing::frame_to_secs`] 的反方向。
    ///
    /// 取的是**当前正在显示的那一帧**：3.4567s、30fps 时返回 103（帧 103 占
    /// [3.4333, 3.4667)），而不是"离得最近的那一帧"。"把当前预览位置设为起点"
    /// 要的是屏幕上那一帧，四舍五入会让它往后偏一帧。
    ///
    /// 末尾加的余量见 [`floor_eps`]：`3.4333… × 30` 有可能算出 102.99999999999999，
    /// 而且传进来的时间本身可能只精确到毫秒。
    ///
    /// 帧率拿不到、秒数为负或非有限时返回 0。
    pub fn secs_to_frame(&self, secs: f64) -> i64 {
        match self {
            Framing::Cfr(r) if r.is_valid() => {
                if !secs.is_finite() || secs < 0.0 {
                    return 0;
                }
                let fps = r.as_f64();
                ((secs * fps) + floor_eps(fps)).floor() as i64
            }
            Framing::Vfr { table, .. } => table.secs_to_frame(secs),
            _ => 0,
        }
    }

    /// `-ss secs` 之后解码端吐出来的第一帧是哪一帧。
    ///
    /// `-ss` 是输入选项，ffmpeg 会把 pts 早于 `secs` 的帧全丢掉，所以第一帧是
    /// "第一个不早于 secs 的帧"，要向上取整；[`Framing::secs_to_frame`] 是向下取整，
    /// 取的是"包含这一时刻的那一帧"。两者只在 `secs` 正好落在帧边界上时相等，
    /// 而那正是从帧号反算出来的时间。一旦有人按"当前位置 + 5 秒"这种算法跳转，
    /// 就会差一帧，而解码端是按这个帧号给后续每一帧编号的——帧号从此整体偏一格。
    pub fn first_frame_at_or_after(&self, secs: f64) -> i64 {
        if !self.is_valid() {
            return 0;
        }
        let f = self.secs_to_frame(secs);
        if self.frame_to_secs(f) < secs - 1e-6 {
            f + 1
        } else {
            f
        }
    }

    /// 总帧数。VFR 是表长；CFR 只能估，调用方传进来的是已经估好的值。
    pub fn frame_count_hint(&self, estimated: i64) -> i64 {
        match self {
            Framing::Vfr { table, .. } => table.len() as i64,
            _ => estimated,
        }
    }

    /// 这个帧号落在已知的帧范围里吗。
    ///
    /// 恒定帧率没有帧表，上界只能由调用方拿总帧数去夹（见 [`Framing::clamp_frame`]），
    /// 所以这里只判非负；可变帧率有表，越界就是越界。
    ///
    /// 挡的是 VFR 越界之后 [`FrameTable::frame_to_secs`] 返回 0 那条路：用户敲一个
    /// 比总帧数大的号，时间框会被悄悄填成 `00:00:00.000`，看着像跳到视频开头。
    pub fn frame_in_range(&self, frame: i64) -> bool {
        match self {
            Framing::Vfr { table, .. } => frame >= 0 && (frame as usize) < table.len(),
            _ => frame >= 0,
        }
    }

    /// 把帧号夹进已知的帧范围。VFR 夹到表里最后一帧；恒定帧率保证不了上界
    /// （没有表），只保证非负。
    pub fn clamp_frame(&self, frame: i64) -> i64 {
        let f = frame.max(0);
        match self {
            Framing::Vfr { table, .. } => f.min(table.len().saturating_sub(1) as i64),
            _ => f,
        }
    }

    /// 缓存计费用。
    pub fn approx_bytes(&self) -> usize {
        match self {
            Framing::Vfr { table, .. } => table.approx_bytes(),
            _ => std::mem::size_of::<Self>(),
        }
    }
}

/// 一次探测的全部结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probe {
    /// 横幅里出现了 `Input #0`
    pub opened: bool,
    /// 容器声明的总时长（秒）
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub audio_codec: Option<String>,
    /// showinfo 拿到的精确帧率；拿不到时是 0/0
    pub fps: Rational,
    pub framing: Framing,
    /// 总帧数。CFR 下是"时长 × 帧率"的估算，VFR 下是表长
    pub nb_frames: i64,
}

impl Default for Framing {
    fn default() -> Self {
        Framing::Unknown
    }
}

impl Probe {
    /// 打不开的文件
    pub fn unopened() -> Self {
        Self::default()
    }

    /// 视频轨是不是被 edit list 整段复制过（"战渣浪"那类文件）。
    ///
    /// `packet_count` 要调用方自己数（那次调用只有 mp4/mov 才做，见
    /// [`repeated_timeline`]），所以从外面传进来而不是存在 `Probe` 里。
    pub fn repeated_timeline(&self, packets: i64) -> Option<RepeatedTimeline> {
        let fps = self.framing.fps_f64();
        if packets <= 0 || fps <= 0.0 || self.duration <= 0.0 {
            return None;
        }
        let actual = packets as f64 / fps;
        // 编两遍的文件实际长度差不多正好是声明的两倍；留 1 秒和 2% 的余量，
        // 免得把容器时长本来就有一点出入的正常文件误判进来
        if actual > self.duration + 1.0 && actual > self.duration * 1.02 {
            Some(RepeatedTimeline {
                declared: self.duration,
                actual,
            })
        } else {
            None
        }
    }
}

/// 视频轨被 edit list 整段复制时的声明长度和实际长度。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RepeatedTimeline {
    pub declared: f64,
    pub actual: f64,
}

/// 用"时长 × 精确帧率"估总帧数。这是 CFR 下唯一能拿到总帧数的办法
/// （ffmpeg 的横幅里根本没有帧数）。
pub fn estimate_frames(duration: f64, fps: Rational) -> i64 {
    if duration <= 0.0 || !fps.is_valid() {
        return 0;
    }
    (duration * fps.as_f64()).round() as i64
}

// ---- 三个原语 ----

/// (A) 读 `ffmpeg -i <file>` 的横幅。
/// 这几趟探测各自最多跑多久。
///
/// 都是毫秒级的活（横幅约 120ms、showinfo 约 150ms、数包实测 100MB 的 mp4 约
/// 290ms），给到这个量级已经是网盘或者坏文件的程度了，再久就是卡住。数包要给得
/// 宽一些：它要扫完整个文件，两小时的大文件在慢盘上确实会久一点。
const BANNER_TIMEOUT: Duration = Duration::from_secs(15);
const SHOWINFO_TIMEOUT: Duration = Duration::from_secs(15);
const PACKET_COUNT_TIMEOUT: Duration = Duration::from_secs(120);

/// 跑一个子进程把它的 stderr 收回来，最多等 `limit` 秒。
///
/// 不用 `Command::output()`：那个等多久没有上限，ffmpeg 一卡住，调用线程和子进程
/// 就都回不来了 —— 探测服务的 `cancel`、关窗口时的强杀都够不着它（够不着是因为
/// pid 没登记，见 ffmpeg.rs 的 `current_pid`）。
///
/// stderr 必须边读边收：管道缓冲区填满之后 ffmpeg 会阻塞在写上，光盯着它退出
/// 就成了死等。
fn capture_stderr(cmd: &mut Command, limit: Duration) -> Option<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;

    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let Ok(reader) = std::thread::Builder::new()
        .name("probe-stderr".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stderr, &mut buf);
            buf
        })
    else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };

    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
        if Instant::now() >= deadline {
            // 到点了就杀掉。读线程会因为管道关闭自然退出。
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    reader.join().ok()
}

/// (A) 读 `ffmpeg -i` 的横幅。
///
/// `ffmpeg -i <file>` 没有输出参数时会立刻报错退出，横幅就在这时的 stderr 上。
pub fn read_banner(ffmpeg: &Path, input: &str) -> Banner {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-i"]).arg(input);

    match capture_stderr(&mut cmd, BANNER_TIMEOUT) {
        Some(out) => parse_banner(&String::from_utf8_lossy(&out)),
        None => Banner::default(),
    }
}

/// (B) 拿精确帧率。
///
/// showinfo 滤镜初始化时会打一行
/// `config in time_base: 1/24000, frame_rate: 24000/1001`，这个 `frame_rate` 实测
/// 恒等于 ffprobe 的 `r_frame_rate`，而且**不挑容器**——它来自流参数（H.264 的 VUI
/// 或容器头），不从容器刻度反推，所以 mkv/flv 的 1 毫秒时间基影响不到它。
///
/// `-frames:v 1` 让它只解一帧就停；`checksum=0` 必须带（默认是 true，解一帧时
/// 无所谓，但全片扫描时差一倍多）。
pub fn exact_frame_rate(ffmpeg: &Path, input: &str) -> Option<Rational> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-an", "-sn", "-dn", "-i"])
        .arg(input)
        .args(["-vf", "showinfo=checksum=0", "-frames:v", "1", "-f", "null", "-"]);

    let out = capture_stderr(&mut cmd, SHOWINFO_TIMEOUT)?;
    parse_showinfo_rate(&String::from_utf8_lossy(&out))
}

/// (C) 拆包拿逐帧时间戳，不解码。
///
/// `-c copy` 是关键：它把包原样搬过去，保留源时间戳。**不加**会变成 ffmpeg 重新
/// 生成时间线，那就完全不对了。
///
/// 输出可能很大（两小时 60fps 能到 20 MB），所以是**边读边解析**，不整个收进内存。
/// `stop` 置位时杀掉子进程并返回 None；`progress` 每读到一条数据行调一次，
/// 参数是已读到的包数（调用方拿它算进度）。
pub fn build_frame_table(
    ffmpeg: &Path,
    input: &str,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(usize),
) -> Option<FrameTable> {
    let mut child = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(input)
        .args(["-map", "0:v:0", "-c", "copy", "-f", "framecrc", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;

    // 下面两条提前返回都要把已经起来的 ffmpeg 杀掉再收尸。Child 的 Drop 不杀进程，
    // 直接 return None 会留下一个没人管的 ffmpeg（capture_stderr 那边就是这么做的）。
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let parser = Arc::new(Mutex::new(FramecrcParser::new()));
    // 读取线程只往通道发"已读多少包"，回调由调用方在本线程调 ——
    // `&mut dyn FnMut` 不是 Send，塞不进子线程。
    let (count_tx, count_rx) = std::sync::mpsc::channel::<usize>();
    let reader = {
        let parser = parser.clone();
        match std::thread::Builder::new()
            .name("framecrc-reader".into())
            .spawn(move || {
                let mut p = parser.lock().unwrap();
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if p.feed(&line) && p.len() % PROGRESS_EVERY == 0 {
                        if count_tx.send(p.len()).is_err() {
                            break;
                        }
                    }
                }
                let _ = count_tx.send(p.len());
            }) {
            Ok(handle) => handle,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };

    // 轮询退出，同时盯着 stop。ffmpeg 卡住时也能被叫停。
    let mut exited_ok = false;
    let killed = loop {
        while let Ok(n) = count_rx.try_recv() {
            progress(n);
        }
        if stop.load(Ordering::SeqCst) {
            let _ = child.kill();
            break true;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                exited_ok = status.success();
                break false;
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                break true;
            }
        }
        std::thread::sleep(POLL);
    };
    child.wait().ok();
    reader.join().ok();
    while let Ok(n) = count_rx.try_recv() {
        progress(n);
    }
    // 被叫停、或者 ffmpeg 半路失败时，手上一张"前缀表"。拿它当完整帧表用的话，
    // 总帧数、VFR 判断、帧号换算全都建立在残缺数据上，比没有表更糟。
    if killed || !exited_ok {
        return None;
    }

    let parser = Arc::try_unwrap(parser).ok()?.into_inner().ok()?;
    parser.finish()
}

/// (D) 数视频包数。edit list 检测（"战渣浪"那类文件）用它。
///
/// **这一趟不能加 `-v error`，也不能加 `-nostats`**——统计行就是数据本身。
pub fn packet_count(ffmpeg: &Path, input: &str) -> Option<i64> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-i"])
        .arg(input)
        .args(["-map", "0:v:0", "-c", "copy", "-f", "null", "-"]);

    let out = capture_stderr(&mut cmd, PACKET_COUNT_TIMEOUT)?;
    parse_packet_count(&String::from_utf8_lossy(&out))
}

/// 一级筛子判出可疑 VFR、开始拆包建帧表时的进度值。
///
/// UI 侧拿它决定要不要弹「正在探测」：走到这个值说明正在做唯一需要用户等的事。
/// 拆包是分段上报的，所以必须有个明确的起点值——只靠"有没有报过进度"分不出来，
/// 包数不到一次上报间隔的文件根本不会报。
pub const VFR_TABLE_STAGE_PERCENT: u8 = 36;

/// 跑一遍完整探测。
///
/// 流程和进度分段（`on_progress` 收到的是 0..=100）：
///
/// 1. 横幅（0 → 15）
/// 2. showinfo 拿精确帧率（15 → 35）
/// 3. **一级筛子**：横幅的 avg 和 tbr 两位小数不同才往下走（免费）
/// 4. **二级判据**：拆包建帧表，然后看"帧数 ÷ 跨度"对不对得上精确帧率
///    （36 → 99，起点见 [`VFR_TABLE_STAGE_PERCENT`]）
///
/// 第 3、4 步是绝大多数文件走不到的地方——osu! 背景视频基本都是 CFR，
/// 两次子进程（约 270 ms）就完事，不用建表。
///
/// `stop` 置位时中止（正在跑的子进程会被杀掉）。
pub fn probe(
    ffmpeg: &Path,
    input: &str,
    stop: &AtomicBool,
    on_progress: &mut dyn FnMut(u8),
) -> Probe {
    on_progress(0);

    let banner = read_banner(ffmpeg, input);
    if !banner.opened {
        return Probe::unopened();
    }
    on_progress(15);

    let fps = exact_frame_rate(ffmpeg, input).unwrap_or_default();
    on_progress(35);

    let estimated = estimate_frames(banner.duration, fps);
    let mut framing = if fps.is_valid() {
        Framing::Cfr(fps)
    } else {
        Framing::Unknown
    };
    let mut nb_frames = estimated;

    // 一级筛子过不了就直接收工。轻微微 FR 分不出来，但那种文件的线性换算误差
    // 本来就在 0.01 帧量级，不值得为它拆一遍包。
    if banner_suspects_vfr(&banner) && fps.is_valid() && !stop.load(Ordering::SeqCst) {
        on_progress(VFR_TABLE_STAGE_PERCENT);
        // 帧表阶段的进度是按"已读包数 ÷ 估出来的总包数"算的。edit list 那类文件
        // 声明时长只有实际的一半，估出来的分母会偏小，所以夹到 99 不冲到 100。
        let total = (estimated.max(1)) as f64;
        let start = VFR_TABLE_STAGE_PERCENT as f64;
        let table = build_frame_table(ffmpeg, input, stop, &mut |n| {
            let pct = start + (n as f64 / total).min(1.0) * (99.0 - start);
            on_progress(pct.min(99.0) as u8);
        });
        if let Some(t) = table {
            if is_vfr(t.len(), t.span_secs(), fps) {
                nb_frames = t.len() as i64;
                framing = Framing::Vfr { fps, table: t };
            }
        }
    }

    on_progress(100);
    Probe {
        opened: true,
        duration: banner.duration,
        width: banner.width,
        height: banner.height,
        audio_codec: banner.audio_codec,
        fps,
        framing,
        nb_frames,
    }
}

/// 数一遍视频包数，用来查 edit list。**只该对 mp4/mov 调**——别的容器没有 edit list，
/// 白花 160–290 ms。
///
/// 和 [`Probe::repeated_timeline`] 配套：先数包，再把包数传进去。
pub fn repeated_timeline(ffmpeg: &Path, input: &str, probe: &Probe) -> Option<RepeatedTimeline> {
    let packets = packet_count(ffmpeg, input)?;
    probe.repeated_timeline(packets)
}

// ---- 判据 ----

/// 一级筛子：横幅里的 `fps`(avg) 和 `tbr`(r) 两位小数不同 → 可疑 VFR。
///
/// 免费——横幅本来就要读。实测真 VFR 文件（avg 19.23 / r 30）当场分出，
/// 所有 CFR 文件两个数一模一样。轻微微 FR 分不出来，但那种文件的线性换算误差
/// 本来就在 0.01 帧量级，无所谓。
pub fn banner_suspects_vfr(b: &Banner) -> bool {
    if !b.opened || b.avg_fps <= 0.0 || b.r_fps <= 0.0 {
        return false;
    }
    // 横幅只印两位小数，比到那一级就够
    (b.avg_fps * 100.0).round() != (b.r_fps * 100.0).round()
}

/// 二级判据：真帧率对不上"帧数 ÷ 跨度"就是 VFR。
///
/// `frames` 是帧表长度（**不是帧数减一**），`span_secs` 是首帧到末帧的秒数，
/// `fps` 是精确帧率。`frames` 帧之间有 `frames - 1` 个间隔，所以是拿它去比。
///
/// 实测 CFR 文件最大 9.85e-05（mkv/flv 的 1 毫秒时间基地板），VFR 文件 3.7e-01。
pub fn is_vfr(frames: usize, span_secs: f64, fps: Rational) -> bool {
    if frames < 2 || !(span_secs > 0.0) || !fps.is_valid() {
        return false;
    }
    let expect = span_secs * fps.as_f64();
    if !(expect > 0.0) {
        return false;
    }
    ((frames - 1) as f64 - expect).abs() / expect > VFR_TOLERANCE
}

// ---- 下面是纯解析，喂字符串就能测，不碰子进程 ----

/// 解析 `ffmpeg -i` 的横幅文本。
pub fn parse_banner(text: &str) -> Banner {
    let mut b = Banner::default();
    b.opened = text.contains("Input #0");
    if !b.opened {
        return b;
    }

    if let Some(c) = duration_re().captures(text) {
        b.duration = hms(&c, 1);
    }

    for line in text.lines() {
        let t = line.trim_start();
        // 只看以 Stream 开头的行：Metadata 里的 handler_name 之类也可能带 "Video:"
        if !t.starts_with("Stream") {
            continue;
        }

        if b.audio_codec.is_none() && t.contains("Audio:") {
            if let Some(c) = audio_re().captures(t) {
                b.audio_codec = Some(c[1].to_string());
            }
        }

        if t.contains("Video:") {
            if b.width == 0 || b.height == 0 {
                // 分辨率正则要求 x 前面至少两位数字，所以编码器的十六进制 tag
                // （avc1 后面那个 0x31637661）匹配不上
                if let Some(c) = dim_re().captures(t) {
                    b.width = c[1].parse().unwrap_or(0);
                    b.height = c[2].parse().unwrap_or(0);
                }
            }
            // 视频流那行形如 `23.98 fps, 23.98 tbr, 24k tbn`。两个数要**分开**抓：
            // ffmpeg 打的是 avg_frame_rate 和 r_frame_rate，它们不相等就是 VFR。
            if b.avg_fps <= 0.0 {
                if let Some(c) = fps_re().captures(t) {
                    b.avg_fps = c[1].parse().unwrap_or(0.0);
                }
            }
            if b.r_fps <= 0.0 {
                if let Some(c) = tbr_re().captures(t) {
                    b.r_fps = c[1].parse().unwrap_or(0.0);
                }
            }
        }
    }

    // 有些容器只打其中一个，两边补齐，免得 `fps()` 和一级筛子其中一个漏了
    if b.avg_fps <= 0.0 {
        b.avg_fps = b.r_fps;
    }
    if b.r_fps <= 0.0 {
        b.r_fps = b.avg_fps;
    }

    b
}

/// 解析 showinfo 的那行 `config in time_base: 1/24000, frame_rate: 24000/1001`。
///
/// `frame_rate` 也可能是裸整数（`frame_rate: 30`）。
pub fn parse_showinfo_rate(text: &str) -> Option<Rational> {
    let c = showinfo_rate_re().captures(text)?;
    let num: u32 = c[1].parse().ok()?;
    match c.get(2) {
        Some(den) => {
            let den: u32 = den.as_str().parse().ok()?;
            let r = Rational::new(num, den);
            r.is_valid().then_some(r)
        }
        None => {
            let r = Rational::new(num, 1);
            r.is_valid().then_some(r)
        }
    }
}

/// 解析 `-c copy -f null -` 的 stderr，取最后一条统计行里的 `frame=N`。
pub fn parse_packet_count(text: &str) -> Option<i64> {
    // 统计行是拿 \r 原地刷新的，最后一条才是总数
    let last = text
        .split(['\r', '\n'])
        .filter(|l| l.contains("frame="))
        .next_back()?;
    frame_re().captures(last)?[1].parse().ok()
}

/// 解析完整的 framecrc 输出。给单测用；跑真文件走 [`build_frame_table`] 的流式路径。
pub fn parse_framecrc_table(text: &str) -> Option<FrameTable> {
    let mut p = FramecrcParser::new();
    for line in text.lines() {
        p.feed(line);
    }
    p.finish()
}

/// framecrc 的增量解析器。头行给 timebase，数据行给 pts。
///
/// 抽出来是为了让流式读取和单测走**同一份**代码。
#[derive(Debug, Default)]
struct FramecrcParser {
    timebase: Option<Rational>,
    /// 解码顺序，`finish()` 里才排序
    pts: Vec<i64>,
}

impl FramecrcParser {
    fn new() -> Self {
        Self::default()
    }

    fn len(&self) -> usize {
        self.pts.len()
    }

    /// 吃一行，返回是否解析到了一条数据行。
    fn feed(&mut self, line: &str) -> bool {
        if line.starts_with("#tb") {
            if let Some(c) = tb_re().captures(line) {
                let (num, den) = (c[1].parse(), c[2].parse());
                if let (Ok(num), Ok(den)) = (num, den) {
                    self.timebase = Some(Rational::new(num, den));
                }
            }
            return false;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            return false;
        }

        // 数据行：stream, dts, pts, duration, size, crc[, ...]
        let mut cols = line.split(',');
        if cols.next().is_none() {
            return false; // stream index，我们只 map 了 0:v:0，只有一个流
        }
        if cols.next().is_none() {
            return false; // dts，用不上（有 B 帧时它不是显示顺序）
        }
        let Some(pts) = cols.next() else {
            return false;
        };
        let Ok(pts) = pts.trim().parse::<i64>() else {
            return false;
        };
        // AVI 存不下 B 帧的 PTS，framecrc 打出来全是 NOPTS。那种文件不建表
        // （AVI 按设计就是恒定帧率），整张表判定失败。
        if pts == i64::MIN {
            self.pts.clear();
            self.timebase = None;
            return false;
        }
        self.pts.push(pts);
        true
    }

    fn finish(mut self) -> Option<FrameTable> {
        let timebase = self.timebase?;
        if !timebase.is_valid() || self.pts.len() < 2 {
            return None;
        }
        // 数据行是解码顺序，有 B 帧时 pts 不单调。排序后才是显示顺序。
        self.pts.sort_unstable();
        Some(FrameTable {
            timebase,
            pts: self.pts,
        })
    }
}

/// 把 `时:分:秒.小数` 三个捕获组换算成秒；`base` 是第一个组的下标。
fn hms(c: &regex::Captures, base: usize) -> f64 {
    let h: f64 = c[base].parse().unwrap_or(0.0);
    let m: f64 = c[base + 1].parse().unwrap_or(0.0);
    let s: f64 = c[base + 2].parse().unwrap_or(0.0);
    h * 3600.0 + m * 60.0 + s
}

fn duration_re() -> Regex {
    Regex::new(r"Duration:\s*(\d+):(\d+):(\d+(?:\.\d+)?)").unwrap()
}

fn fps_re() -> Regex {
    Regex::new(r"([0-9]+(?:\.[0-9]+)?)\s*fps").unwrap()
}

fn tbr_re() -> Regex {
    Regex::new(r"([0-9]+(?:\.[0-9]+)?)\s*tbr").unwrap()
}

fn dim_re() -> Regex {
    Regex::new(r"(\d{2,5})x(\d{2,5})").unwrap()
}

fn audio_re() -> Regex {
    Regex::new(r"Audio:\s*([A-Za-z0-9_]+)").unwrap()
}

/// `config in time_base: 1/24000, frame_rate: 24000/1001`，分母可缺省。
fn showinfo_rate_re() -> Regex {
    Regex::new(r"frame_rate:\s*(\d+)(?:/(\d+))?").unwrap()
}

fn tb_re() -> Regex {
    Regex::new(r"#tb\s*\d+:\s*(\d+)/(\d+)").unwrap()
}

fn frame_re() -> Regex {
    Regex::new(r"frame=\s*(\d+)").unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1280x720 的 mp4，带 aac 音轨，29.97（摘自真实运行）
    const BANNER_MP4: &str = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':
  Metadata:
    major_brand     : isom
    handler_name    : VideoHandler
  Duration: 00:02:54.61, start: 0.000000, bitrate: 4773 kb/s
  Stream #0:0[0x1](eng): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 1280x720 [SAR 1:1 DAR 16:9], 4582 kb/s, 29.97 fps, 29.97 tbr, 90k tbn (default)
  Stream #0:1[0x2](eng): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, stereo, fltp, 189 kb/s (default)
";

    /// 打不开的文件：没有 Input #0
    const BANNER_FAILED: &str = "\
x.mp4: No such file or directory
Error opening input file x.mp4.
Error opening input files: No such file or directory
";

    #[test]
    fn banner_parses_mp4() {
        let b = parse_banner(BANNER_MP4);
        assert!(b.opened);
        assert!((b.duration - 174.61).abs() < 1e-9, "duration = {}", b.duration);
        assert!((b.avg_fps - 29.97).abs() < 1e-9, "avg = {}", b.avg_fps);
        assert!((b.r_fps - 29.97).abs() < 1e-9, "r = {}", b.r_fps);
        assert_eq!((b.width, b.height), (1280, 720));
        assert_eq!(b.audio_codec.as_deref(), Some("aac"));
    }

    #[test]
    fn banner_detects_open_failure() {
        let b = parse_banner(BANNER_FAILED);
        assert!(!b.opened);
        assert_eq!(b, Banner::default());
    }

    #[test]
    fn banner_accepts_duration_na() {
        let text = BANNER_MP4.replace("Duration: 00:02:54.61", "Duration: N/A");
        let b = parse_banner(&text);
        assert!(b.opened);
        assert_eq!(b.duration, 0.0);
        assert_eq!(b.width, 1280, "时长读不出来不该影响别的字段");
    }

    #[test]
    fn banner_does_not_mistake_codec_tag_for_resolution() {
        // 只有编码器那一行，没有分辨率：0x31637661 不能被当成 316x37661 之类
        let text = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p, 30 fps, 30 tbr
";
        let b = parse_banner(text);
        assert_eq!((b.width, b.height), (0, 0));
        assert_eq!(b.avg_fps, 30.0);
    }

    #[test]
    fn banner_keeps_avg_and_r_separate() {
        // 真 VFR 文件（实测）：两个数不一样，一级筛子要能认出来
        let text = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':
  Duration: 00:00:03.90, start: 0.000000, bitrate: 39 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 320x240 [SAR 1:1 DAR 4:3], 35 kb/s, 19.23 fps, 30 tbr, 15360 tbn (default)
";
        let b = parse_banner(text);
        assert!((b.avg_fps - 19.23).abs() < 1e-9, "avg = {}", b.avg_fps);
        assert!((b.r_fps - 30.0).abs() < 1e-9, "r = {}", b.r_fps);
        assert!(banner_suspects_vfr(&b));
        // fps() 是给不挑精度的地方用的，优先 avg
        assert!((b.fps() - 19.23).abs() < 1e-9);
    }

    #[test]
    fn banner_only_tbr_fills_both() {
        // 只有 tbr 没有 fps 时，两边都填上，免得 fps() 拿到 0
        let text = "\
Input #0, matroska,webm, from 'x.mkv':
  Stream #0:0: Video: h264, yuv420p, 640x480, 23.98 tbr
";
        let b = parse_banner(text);
        assert!((b.r_fps - 23.98).abs() < 1e-9);
        assert!((b.avg_fps - 23.98).abs() < 1e-9);
        assert!(!banner_suspects_vfr(&b), "两边补齐之后不该再报可疑 VFR");
    }

    #[test]
    fn banner_audio_only_file_has_no_video_fields() {
        let text = "\
Input #0, mp3, from 'x.mp3':
  Duration: 00:03:20.00, start: 0.000000, bitrate: 128 kb/s
  Stream #0:0: Audio: mp3, 44100 Hz, stereo, fltp, 128 kb/s
";
        let b = parse_banner(text);
        assert!(b.opened);
        assert_eq!(b.audio_codec.as_deref(), Some("mp3"));
        assert_eq!((b.width, b.height), (0, 0));
        assert_eq!(b.avg_fps, 0.0);
        assert!(!banner_suspects_vfr(&b), "没有帧率时不能报可疑 VFR");
    }

    #[test]
    fn banner_without_audio_stream_reports_none() {
        let text = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':
  Duration: 00:00:08.00, start: 0.000000, bitrate: 52 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p, 640x480, 30 fps, 30 tbr
";
        let b = parse_banner(text);
        assert_eq!(b.audio_codec, None);
    }

    #[test]
    fn suspects_vfr_false_for_matching_two_decimals() {
        // 实测：mkv/flv/avi 的 avg 和 r 都是 23.98，是 CFR
        let mut b = parse_banner(BANNER_MP4);
        b.avg_fps = 23.98;
        b.r_fps = 23.98;
        assert!(!banner_suspects_vfr(&b));

        // 整数帧率也是 CFR
        b.avg_fps = 30.0;
        b.r_fps = 30.0;
        assert!(!banner_suspects_vfr(&b));
    }

    #[test]
    fn showinfo_rate_gives_exact_ntsc() {
        // 摘自真实运行：fps23976.mp4
        let text = "[Parsed_showinfo_0 @ 0000025228411300] config in time_base: 1/24000, frame_rate: 24000/1001\n";
        let r = parse_showinfo_rate(text).unwrap();
        assert_eq!(r, Rational::new(24000, 1001));
        assert!((r.as_f64() - 24000.0 / 1001.0).abs() < 1e-12);
    }

    #[test]
    fn showinfo_rate_accepts_bare_integer() {
        let text = "[Parsed_showinfo_0 @ x] config in time_base: 1/15360, frame_rate: 30\n";
        assert_eq!(parse_showinfo_rate(text), Some(Rational::new(30, 1)));
    }

    #[test]
    fn showinfo_rate_rejects_garbage() {
        assert_eq!(parse_showinfo_rate(""), None);
        assert_eq!(parse_showinfo_rate("frame_rate: 0/0"), None, "0/0 是无效的");
        assert_eq!(parse_showinfo_rate("frame_rate: 24000/0"), None);
    }

    /// 摘自真实运行：fps23976.mp4（有 B 帧，数据行是解码顺序）
    const FRAMECRC_MP4: &str = "\
#extradata 0:       44, 0x43c110e8
#software: Lavf62.19.101
#tb 0: 1/24000
#media_type 0: video
#codec_id 0: h264
#dimensions 0: 320x240
#sar 0: 1/1
0,      -2002,          0,     1001,     3968, 0xb9354d23
0,      -1001,       4004,     1001,      511, 0x143302a2, F=0x0
0,          0,       2002,     1001,       98, 0xd35d2c0b, F=0x0
0,       1001,       1001,     1001,       38, 0x3cbc1278, F=0x0
0,       2002,       3003,     1001,       31, 0xa6230cec, F=0x0
";

    #[test]
    fn framecrc_sorts_into_display_order() {
        let t = parse_framecrc_table(FRAMECRC_MP4).unwrap();
        assert_eq!(t.timebase, Rational::new(1, 24000));
        // 解码顺序是 0, 4004, 2002, 1001, 3003；显示顺序要排好
        assert_eq!(t.pts, vec![0, 1001, 2002, 3003, 4004]);
    }

    #[test]
    fn framecrc_frame_to_secs_is_exact() {
        let t = parse_framecrc_table(FRAMECRC_MP4).unwrap();
        assert_eq!(t.frame_to_secs(0), 0.0);
        assert!((t.frame_to_secs(4) - 4004.0 / 24000.0).abs() < 1e-12);
        assert_eq!(t.frame_to_secs(99), 0.0, "越界返回 0，不 panic");
    }

    #[test]
    fn framecrc_secs_to_frame_picks_the_displayed_frame() {
        let t = parse_framecrc_table(FRAMECRC_MP4).unwrap();
        // 帧 i 占 [i*1001/24000, (i+1)*1001/24000)。取正在显示的那一帧。
        assert_eq!(t.secs_to_frame(0.0), 0);
        assert_eq!(t.secs_to_frame(1001.0 / 24000.0), 1);
        // 帧边界下方半个毫秒（这个时间基上是 12 个刻度）按下一帧算，见 floor_eps。
        // 第 0 帧占 [0, 1001) 刻度，989 已经贴到上界了，舍入误差能把它推过去。
        assert_eq!(t.secs_to_frame(989.0 / 24000.0), 1, "边界下方半毫秒内算下一帧");
        assert_eq!(t.secs_to_frame(988.0 / 24000.0), 0, "再退一格就还是这一帧");
        assert_eq!(t.secs_to_frame(900.0 / 24000.0), 0, "明显在区间里，算这一帧");
        assert_eq!(t.secs_to_frame(1e9), 4, "超出末尾落到最后一帧");
        assert_eq!(t.secs_to_frame(-1.0), 0, "负数是无效输入");
        assert_eq!(t.secs_to_frame(f64::NAN), 0);
    }

    #[test]
    fn framecrc_span_is_first_to_last() {
        let t = parse_framecrc_table(FRAMECRC_MP4).unwrap();
        assert!((t.span_secs() - 4004.0 / 24000.0).abs() < 1e-12);
    }

    #[test]
    fn framecrc_rejects_nopts_pts() {
        // AVI：pts 全是 NOPTS，只有 dts。这种文件不建表。
        let text = "\
#tb 0: 1001/24000
0,          0, -9223372036854775808,        1,     4003, 0x4645572c
0,          1, -9223372036854775808,        1,      511, 0x220001a7
";
        assert_eq!(parse_framecrc_table(text), None);
    }

    #[test]
    fn framecrc_rejects_garbage() {
        assert_eq!(parse_framecrc_table(""), None, "什么都没有");
        assert_eq!(parse_framecrc_table("#tb 0: 1/24000\n"), None, "只有头没有数据行");
        assert_eq!(
            parse_framecrc_table("#tb 0: 0/0\n0, 0, 0, 1, 1, 0xff\n0, 1, 1, 1, 1, 0xff\n"),
            None,
            "无效时间基"
        );
        // 只有一帧的表没有意义（跨度为 0）
        assert_eq!(parse_framecrc_table("#tb 0: 1/24000\n0, 0, 0, 1001, 1, 0xff\n"), None);
    }

    #[test]
    fn vfr_detector_separates_the_two_populations() {
        // CFR 的地板：mkv/flv 的 1 毫秒时间基，实测相对差 9.85e-05
        // （72 帧，71 个间隔，跨度 × 帧率 = 70.99）
        assert!(!is_vfr(72, 70.99 / (24000.0 / 1001.0), Rational::new(24000, 1001)));
        // 真 VFR：实测 75 帧、74 个间隔，跨度 × 帧率 = 118，相对差 3.7e-01
        assert!(is_vfr(75, 118.0 / 30.0, Rational::new(30, 1)));
        // 恰好对上的是 CFR（76 帧，75 个间隔）
        assert!(!is_vfr(76, 75.0 / 30.0, Rational::new(30, 1)));
    }

    #[test]
    fn vfr_detector_refuses_degenerate_input() {
        assert!(!is_vfr(1, 1.0, Rational::new(30, 1)), "不足两帧");
        assert!(!is_vfr(10, 0.0, Rational::new(30, 1)), "跨度为 0");
        assert!(!is_vfr(10, 1.0, Rational::new(0, 0)), "帧率无效");
        assert!(!is_vfr(10, f64::NAN, Rational::new(30, 1)));
    }

    #[test]
    fn packet_count_reads_last_progress_line() {
        // 统计行是拿 \r 刷新的，只有最后一条是总数
        let text = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':
  Duration: 00:02:54.61, start: 0.000000, bitrate: 4773 kb/s
frame=  100 fps=0.0 time=00:00:03.34 bitrate=N/A speed=1e+03x\rframe=10466 fps=0.0 Lsize=N/A time=00:05:49.21 bitrate=N/A speed=1.55e+03x
";
        assert_eq!(parse_packet_count(text), Some(10466));
    }

    #[test]
    fn packet_count_none_without_progress_line() {
        assert_eq!(parse_packet_count("Input #0, foo, from 'x':\n"), None);
    }

    #[test]
    fn rational_as_f64_guards_zero_den() {
        assert_eq!(Rational::new(30, 0).as_f64(), 0.0);
        assert!(!Rational::new(30, 0).is_valid());
        assert!((Rational::new(30000, 1001).as_f64() - 29.97002997).abs() < 1e-8);
    }

    #[test]
    fn frame_table_approx_bytes_counts_the_vec() {
        let t = parse_framecrc_table(FRAMECRC_MP4).unwrap();
        assert!(t.approx_bytes() >= 5 * 8);
    }

    // ---- Framing ----

    #[test]
    fn framing_cfr_does_the_same_math_as_before() {
        // 23.976 用有理数表示，算出来的和旧的 f64 版本一致
        let f = Framing::Cfr(Rational::new(24000, 1001));
        assert!((f.frame_to_secs(240) - 240.0 / (24000.0 / 1001.0)).abs() < 1e-9);
        assert_eq!(f.frame_to_secs(0), 0.0);

        // 帧 103 占 [3.4333, 3.4667)，3.4567 落在这区间里
        let g = Framing::Cfr(Rational::new(30, 1));
        assert_eq!(g.secs_to_frame(3.4567), 103);
        assert_eq!(g.secs_to_frame(103.0 / 30.0), 103, "帧边界上不能因浮点误差退回上一帧");
    }

    #[test]
    fn cfr_secs_to_frame_survives_millisecond_rounding() {
        // 时间框只写到毫秒。24fps 下一帧 41.667 毫秒，真实帧边界被就近舍入之后可能
        // 落到边界下方，floor 就会退回上一帧——用户报的"按下一帧帧号不动"。
        let f = Framing::Cfr(Rational::new(24, 1));
        // 3161/24 = 131.70833…，舍到毫秒是 131.708
        assert_eq!(f.secs_to_frame(131.708), 3161);
        // 3160/24 = 131.66667…，舍到毫秒是 131.667
        assert_eq!(f.secs_to_frame(131.667), 3160);

        // 连着十帧，每帧都舍到毫秒再换算，一次都不许错。
        // 24fps 上每隔三帧就会撞到一次边界，修之前正是这三种位置出问题。
        for n in 3160..3170 {
            let rounded_to_ms = ((n as f64) / 24.0 * 1000.0).round() / 1000.0;
            assert_eq!(f.secs_to_frame(rounded_to_ms), n, "第 {n} 帧");
        }
    }

    #[test]
    fn framing_cfr_round_trips() {
        let f = Framing::Cfr(Rational::new(30, 1));
        for n in [0_i64, 1, 29, 240, 1000] {
            assert_eq!(f.secs_to_frame(f.frame_to_secs(n)), n);
        }
    }

    #[test]
    fn framing_cfr_guards_bad_input() {
        let f = Framing::Cfr(Rational::new(30, 1));
        assert_eq!(f.frame_to_secs(-1), 0.0, "负帧号");
        assert_eq!(f.secs_to_frame(-3.0), 0);
        assert_eq!(f.secs_to_frame(f64::NAN), 0);
        assert_eq!(f.secs_to_frame(f64::INFINITY), 0);
    }

    #[test]
    fn framing_vfr_goes_through_the_table() {
        // 实测的 VFR 素材：三种帧间隔，用线性换算是错的
        let table = FrameTable {
            timebase: Rational::new(1, 15360),
            pts: vec![0, 512, 1024, 1536, 2048, 7168],
        };
        let f = Framing::Vfr {
            fps: Rational::new(30, 1),
            table,
        };
        assert!(f.is_valid());
        // 第 5 帧在第 7168 刻度处，不是"第 5 帧 × 1/30 秒"
        assert!((f.frame_to_secs(5) - 7168.0 / 15360.0).abs() < 1e-12);
        assert_eq!(f.secs_to_frame(7168.0 / 15360.0), 5);
        // 线性换算会给 5/30 = 0.1667s，表给 0.4667s —— 差得远，这正是要建表的理由
        assert!((f.frame_to_secs(5) - 5.0 / 30.0).abs() > 0.3);
    }

    #[test]
    fn framing_unknown_never_converts() {
        let f = Framing::Unknown;
        assert!(!f.is_valid());
        assert_eq!(f.frame_to_secs(10), 0.0);
        assert_eq!(f.secs_to_frame(1.0), 0);
        assert_eq!(f.fps(), Rational::default());
        assert!(!f.fps().is_valid());
    }

    #[test]
    fn framing_invalid_rational_is_not_valid() {
        assert!(!Framing::Cfr(Rational::new(0, 0)).is_valid());
        assert!(!Framing::Cfr(Rational::new(30, 0)).is_valid());
        assert_eq!(Framing::Cfr(Rational::new(30, 0)).secs_to_frame(5.0), 0);
    }

    #[test]
    fn frame_count_hint_prefers_the_table() {
        let table = FrameTable {
            timebase: Rational::new(1, 24000),
            pts: vec![0, 1001, 2002],
        };
        let vfr = Framing::Vfr {
            fps: Rational::new(24000, 1001),
            table,
        };
        assert_eq!(vfr.frame_count_hint(999), 3, "VFR 有表就信表");
        assert_eq!(Framing::Cfr(Rational::new(30, 1)).frame_count_hint(999), 999);
    }

    #[test]
    fn out_of_range_frames_are_clamped_on_vfr() {
        let vfr = Framing::Vfr {
            fps: Rational::new(30, 1),
            table: FrameTable {
                timebase: Rational::new(1, 15360),
                pts: vec![0, 512, 1024, 1536, 2048, 7168],
            },
        };
        assert!(vfr.frame_in_range(5));
        assert!(!vfr.frame_in_range(6), "表长 6，第 6 帧（下标 6）不存在");
        assert!(!vfr.frame_in_range(-1));
        // 越界的夹到最后一帧，而不是掉回 0 —— 掉回 0 会让裁剪区间静默变成
        // 「从视频开头开始」
        assert_eq!(vfr.clamp_frame(6), 5);
        assert_eq!(vfr.clamp_frame(99999), 5);
        assert!((vfr.frame_to_secs(vfr.clamp_frame(99999)) - 7168.0 / 15360.0).abs() < 1e-12);

        // 恒定帧率没有表，上界判断不了，只保证非负
        let cfr = Framing::Cfr(Rational::new(30, 1));
        assert!(cfr.frame_in_range(99999));
        assert_eq!(cfr.clamp_frame(99999), 99999);
        assert_eq!(cfr.clamp_frame(-3), 0);
    }

    #[test]
    fn secs_to_frame_does_not_overflow_on_absurd_seconds() {
        // 时间框里的秒数没有上限。首帧 pts 不为 0 时，`first + 大数` 以前会溢出
        // （debug 构建 panic，release 回绕成负数、partition_point 退回第 0 帧）。
        let f = Framing::Vfr {
            fps: Rational::new(30, 1),
            table: FrameTable {
                timebase: Rational::new(1, 15360),
                pts: vec![512, 1024, 1536],
            },
        };
        assert_eq!(f.secs_to_frame(1e300), 2, "夹到最后一帧");
    }

    // ---- Probe ----

    #[test]
    fn estimate_frames_rounds_to_nearest() {
        assert_eq!(estimate_frames(8.0, Rational::new(30, 1)), 240);
        assert_eq!(estimate_frames(174.61, Rational::new(30000, 1001)), 5233);
        assert_eq!(estimate_frames(0.0, Rational::new(30, 1)), 0);
        assert_eq!(estimate_frames(8.0, Rational::default()), 0, "帧率拿不到就别估");
    }

    #[test]
    fn repeated_timeline_flags_a_doubled_track() {
        // 实测的青空之泪：声明 174.61s，实际 10466 帧 / 29.97003 = 349.2s
        let p = Probe {
            opened: true,
            duration: 174.61,
            fps: Rational::new(30000, 1001),
            framing: Framing::Cfr(Rational::new(30000, 1001)),
            ..Probe::default()
        };
        let r = p.repeated_timeline(10466).unwrap();
        assert!((r.declared - 174.61).abs() < 1e-9);
        assert!((r.actual - 349.2).abs() < 0.5);
    }

    #[test]
    fn repeated_timeline_ignores_normal_files() {
        // demo_43：8.000s / 240 帧 / 30fps，实际和声明一致
        let p = Probe {
            opened: true,
            duration: 8.0,
            fps: Rational::new(30, 1),
            framing: Framing::Cfr(Rational::new(30, 1)),
            ..Probe::default()
        };
        assert_eq!(p.repeated_timeline(240), None);
    }

    #[test]
    fn repeated_timeline_refuses_degenerate_input() {
        let p = Probe {
            opened: true,
            duration: 0.0,
            fps: Rational::new(30, 1),
            framing: Framing::Cfr(Rational::new(30, 1)),
            ..Probe::default()
        };
        assert_eq!(p.repeated_timeline(100), None, "声明时长为 0");
        assert_eq!(p.repeated_timeline(0), None, "数不出包");
    }

    #[test]
    fn unopened_probe_has_nothing() {
        let p = Probe::unopened();
        assert!(!p.opened);
        assert!(!p.framing.is_valid());
        assert_eq!(p.nb_frames, 0);
    }
}
