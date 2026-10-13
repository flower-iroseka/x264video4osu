//! 后台跑元数据探测。
//!
//! 探测要起子进程（横幅约 120 ms、showinfo 约 150 ms，可疑 VFR 还要拆一遍包），
//! 直接放在 UI 线程上会把界面冻住。这里照 `FfmpegService` 那套来：worker 线程
//! 跑活、mpsc 回事件、UI 侧在现有的 100 ms 轮询里收。
//!
//! 一次只跑一个——用户换文件时把上一个叫停（子进程会被杀掉）。

use crate::services::ffmpeg_config;
use crate::services::probe::{self, Probe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

/// 后台探测发给 UI 的事件。每个都带 `path`：用户中途换了文件时，
/// 迟到的结果要被丢掉。
#[derive(Debug, Clone)]
pub enum ProbeEvent {
    /// 探测进度，0..=100
    Progress { path: String, percent: u8 },
    Done { path: String, probe: Arc<Probe> },
    /// 文件打不开（没有视频轨不算失败，那种只是帧率未知）
    Failed { path: String },
}

struct Running {
    path: String,
    stop: Arc<AtomicBool>,
}

/// 探测服务的句柄。持有它就能发请求；事件从 [`ProbeService::new`] 返回的
/// receiver 上收。
pub struct ProbeService {
    tx: Sender<ProbeEvent>,
    running: Mutex<Option<Running>>,
}

impl ProbeService {
    pub fn new() -> (Self, Receiver<ProbeEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            Self {
                tx,
                running: Mutex::new(None),
            },
            rx,
        )
    }

    /// 请探测某个文件。已经在探同一个文件就什么都不做（100 ms 轮询会重复调）。
    pub fn request(&self, path: &str) {
        if path.is_empty() {
            return;
        }
        {
            let running = self.running.lock().unwrap();
            if running.as_ref().is_some_and(|r| r.path == path) {
                return;
            }
        }
        self.cancel();

        let stop = Arc::new(AtomicBool::new(false));
        *self.running.lock().unwrap() = Some(Running {
            path: path.to_string(),
            stop: stop.clone(),
        });

        let tx = self.tx.clone();
        let owned = path.to_string();
        let spawned = std::thread::Builder::new()
            .name("probe-worker".into())
            .spawn(move || {
                let ffmpeg = ffmpeg_config::ffmpeg_path();
                let result = probe::probe(&ffmpeg, &owned, &stop, &mut |percent| {
                    let _ = tx.send(ProbeEvent::Progress {
                        path: owned.clone(),
                        percent,
                    });
                });
                if stop.load(Ordering::SeqCst) {
                    return; // 被叫停了，结果已经没用
                }
                if result.opened {
                    let _ = tx.send(ProbeEvent::Done {
                        path: owned,
                        probe: Arc::new(result),
                    });
                } else {
                    let _ = tx.send(ProbeEvent::Failed { path: owned });
                }
            });

        if spawned.is_err() {
            // 线程起不来就当没请求过，免得 UI 一直等一个永远不会来的事件
            self.running.lock().unwrap().take();
        }
    }

    /// 叫停正在跑的探测。已经在读的管道会关掉，子进程被杀。
    pub fn cancel(&self) {
        if let Some(r) = self.running.lock().unwrap().take() {
            r.stop.store(true, Ordering::SeqCst);
        }
    }

    /// 探测跑完了（不管成没成），把"正在跑"这一格清掉。
    pub fn finish(&self, path: &str) {
        let mut running = self.running.lock().unwrap();
        if running.as_ref().is_some_and(|r| r.path == path) {
            running.take();
        }
    }
}
