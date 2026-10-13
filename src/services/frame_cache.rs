//! 一个 session 内探测结果的缓存。
//!
//! 同一个文件在一次运行里会被问好几次（预览开面板、切裁剪单位、点开始编码），
//! 而探测要起子进程。这里把结果留在内存里，重复问就直接返回。
//!
//! **只放内存，不落盘。** 落盘要面对"ffmpeg 换版本之后缓存的数值就过期了"的问题
//! （帧率、总帧数这些都是解析 ffmpeg 的输出得来的），为省一次几百毫秒不值得。

use crate::services::probe::Probe;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// 内容总量上限。
///
/// 典型 osu! 背景视频是 CFR，一个条目不到一百字节；真 VFR 才要存逐帧表
/// （两小时 60fps 约 3.5 MB）。32 MB 能装下约 9 个极端的大 VFR，或者上千个
/// 普通背景视频。
pub const DEFAULT_CAP_BYTES: usize = 32 * 1024 * 1024;

/// 条数上限。防的是几十万个 CFR 小条目把 HashMap 和淘汰扫描撑大。
pub const DEFAULT_CAP_ENTRIES: usize = 64;

/// 文件身份。同名路径被换成别的文件时靠它认出来。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    len: u64,
    mtime: Option<SystemTime>,
}

impl Identity {
    fn of(path: &Path) -> Option<Self> {
        let md = std::fs::metadata(path).ok()?;
        Some(Self {
            len: md.len(),
            mtime: md.modified().ok(),
        })
    }
}

struct Entry {
    identity: Identity,
    probe: Arc<Probe>,
    /// 越大越新
    last_used: u64,
    size: usize,
}

/// 路径 + 文件身份 → 探测结果。超过上限就按最久没用过的淘汰。
pub struct FrameCache {
    entries: HashMap<PathBuf, Entry>,
    tick: u64,
    bytes: usize,
    cap_bytes: usize,
    cap_entries: usize,
}

impl Default for FrameCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAP_BYTES, DEFAULT_CAP_ENTRIES)
    }
}

impl FrameCache {
    pub fn new(cap_bytes: usize, cap_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            tick: 0,
            bytes: 0,
            cap_bytes,
            cap_entries: cap_entries.max(1),
        }
    }

    /// 取缓存。文件名下的内容变过就当没命中（顺手把旧的丢掉）。
    ///
    /// 命中会刷新"最近用过"的记号。
    pub fn get(&mut self, path: &Path) -> Option<Arc<Probe>> {
        let identity = Identity::of(path)?;
        let stale = match self.entries.get(path) {
            Some(e) => e.identity != identity,
            None => return None,
        };
        if stale {
            self.remove(path);
            return None;
        }

        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.get_mut(path)?;
        entry.last_used = tick;
        Some(entry.probe.clone())
    }

    /// 放进去。文件 stat 不到就不缓存——后面没法确认还是不是同一个文件。
    pub fn insert(&mut self, path: &Path, probe: Arc<Probe>) {
        let Some(identity) = Identity::of(path) else {
            return;
        };
        let size = entry_size(path, &probe);
        self.remove(path);
        self.tick += 1;
        self.entries.insert(
            path.to_path_buf(),
            Entry {
                identity,
                probe,
                last_used: self.tick,
                size,
            },
        );
        self.bytes += size;
        self.evict();
    }

    // ---- 下面四个是给测试和排查用的取值方法，生产路径不需要 ----

    /// 缓存里现在有几个文件。
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 内容总共占多少字节。
    #[allow(dead_code)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn remove(&mut self, path: &Path) {
        if let Some(old) = self.entries.remove(path) {
            self.bytes = self.bytes.saturating_sub(old.size);
        }
    }

    /// 超限就一直淘汰最久没用的那个。刚放进去的 `last_used` 最大，不会被自己淘汰掉。
    ///
    /// 只剩一条时不再淘汰——单个大文件（两小时 60fps 的 VFR 表约 3.5 MB）也可能
    /// 超过额度，淘汰成空的话那个文件的表每次都要重建，反而更糟。
    fn evict(&mut self) {
        while self.entries.len() > 1
            && (self.bytes > self.cap_bytes || self.entries.len() > self.cap_entries)
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(p, _)| p.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
    }
}

/// 一个条目占多少字节。帧表是大头，路径和 Entry 本身也要算。
fn entry_size(path: &Path, probe: &Probe) -> usize {
    std::mem::size_of::<Entry>()
        + path.as_os_str().len()
        + std::mem::size_of::<Probe>()
        + probe.audio_codec.as_ref().map_or(0, |s| s.capacity())
        + probe.framing.approx_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::probe::{FrameTable, Framing, Rational};
    use std::io::Write;

    /// 在 `tests-001/` 下造一个文件，返回路径。
    ///
    /// 刻意不落系统临时目录——项目的规矩是所有测试产物都留在项目自己的
    /// `tests-001/` 里（那个目录是 throwaway，也在 .gitignore 里）。
    fn temp_file(name: &str, content: &[u8]) -> PathBuf {
        let dir = PathBuf::from("tests-001").join("frame_cache_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        f.sync_all().unwrap();
        path
    }

    fn clean(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    fn probe_with_table(frames: usize) -> Arc<Probe> {
        Arc::new(Probe {
            opened: true,
            duration: 10.0,
            width: 320,
            height: 240,
            audio_codec: None,
            fps: Rational::new(30, 1),
            framing: Framing::Vfr {
                fps: Rational::new(30, 1),
                table: FrameTable {
                    timebase: Rational::new(1, 15360),
                    pts: (0..frames as i64).map(|i| i * 512).collect(),
                },
            },
            nb_frames: frames as i64,
        })
    }

    #[test]
    fn stores_and_returns() {
        let f = temp_file("a.bin", b"hello");
        let mut c = FrameCache::default();
        assert!(c.get(&f).is_none(), "还没放进去");

        c.insert(&f, probe_with_table(4));
        let got = c.get(&f).unwrap();
        assert_eq!(got.nb_frames, 4);
        assert_eq!(c.len(), 1);
        assert!(c.bytes() > 0);
        clean(&f);
    }

    #[test]
    fn size_change_invalidates_the_entry() {
        let f = temp_file("b.bin", b"hello");
        let mut c = FrameCache::default();
        c.insert(&f, probe_with_table(4));
        assert!(c.get(&f).is_some());

        // 同名路径换成别的文件：长度变了，旧结果不能再用
        std::fs::write(&f, b"a much longer content").unwrap();
        assert!(c.get(&f).is_none());
        assert!(c.is_empty(), "过期的条目要顺手丢掉");
        clean(&f);
    }

    #[test]
    fn missing_file_is_a_miss() {
        let mut c = FrameCache::default();
        let missing = PathBuf::from("tests-001").join("frame_cache_test/definitely-missing.bin");
        let _ = std::fs::remove_file(&missing);
        assert!(c.get(&missing).is_none());
    }

    #[test]
    fn insert_of_a_missing_file_is_ignored() {
        // stat 不到就没法确认身份，与其存一个永远校验不了的条目，不如不存
        let mut c = FrameCache::default();
        let missing = PathBuf::from("tests-001").join("frame_cache_test/definitely-missing2.bin");
        let _ = std::fs::remove_file(&missing);
        c.insert(&missing, probe_with_table(4));
        assert!(c.is_empty());
    }

    #[test]
    fn reinsert_replaces_and_keeps_accounting_right() {
        let f = temp_file("c.bin", b"hello");
        let mut c = FrameCache::default();
        c.insert(&f, probe_with_table(100));
        let after_first = c.bytes();
        c.insert(&f, probe_with_table(4));
        assert_eq!(c.len(), 1);
        assert!(c.bytes() < after_first, "换成小表之后账要减下去");
        assert_eq!(c.get(&f).unwrap().nb_frames, 4);
        clean(&f);
    }

    #[test]
    fn evicts_the_least_recently_used() {
        let a = temp_file("ev-a.bin", b"a");
        let b = temp_file("ev-b.bin", b"b");
        let c_path = temp_file("ev-c.bin", b"c");
        // 只容得下两条
        let mut c = FrameCache::new(DEFAULT_CAP_BYTES, 2);

        c.insert(&a, probe_with_table(4));
        c.insert(&b, probe_with_table(4));
        // 摸一下 a，让 b 变成最久没用的
        assert!(c.get(&a).is_some());

        c.insert(&c_path, probe_with_table(4));
        assert_eq!(c.len(), 2);
        assert!(c.get(&a).is_some(), "刚用过的要留着");
        assert!(c.get(&b).is_none(), "最久没用的被淘汰");
        assert!(c.get(&c_path).is_some());

        for p in [&a, &b, &c_path] {
            clean(p);
        }
    }

    #[test]
    fn evicts_when_over_the_byte_cap() {
        let a = temp_file("cap-a.bin", b"a");
        let b = temp_file("cap-b.bin", b"b");
        // 一个条目几百字节，只给 1 字节的额度
        let mut c = FrameCache::new(1, DEFAULT_CAP_ENTRIES);
        c.insert(&a, probe_with_table(4));
        c.insert(&b, probe_with_table(4));
        assert_eq!(c.len(), 1, "字节超了要淘汰到只剩一条");
        assert!(c.get(&b).is_some(), "刚放进去的留着");
        assert!(c.get(&a).is_none(), "旧的被淘汰");
        clean(&a);
        clean(&b);
    }

    #[test]
    fn a_single_entry_larger_than_the_cap_still_fits() {
        // 淘汰不能让缓存变成永远空着：装不下也得留一条，
        // 否则一个大文件的表每次都重建
        let a = temp_file("big.bin", b"a");
        let mut c = FrameCache::new(1, DEFAULT_CAP_ENTRIES);
        c.insert(&a, probe_with_table(1000));
        assert_eq!(c.len(), 1, "只有一条时不该被淘汰掉");
        assert!(c.get(&a).is_some());
        clean(&a);
    }

    #[test]
    fn clear_empties_everything() {
        let f = temp_file("d.bin", b"hello");
        let mut c = FrameCache::default();
        c.insert(&f, probe_with_table(4));
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.bytes(), 0);
        assert!(c.get(&f).is_none());
        clean(&f);
    }
}
