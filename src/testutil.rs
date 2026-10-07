//! 测试用的临时文件目录。
//!
//! 测试需要往磁盘上放东西（绕过校验的假输入、真的小视频），一律放这里 ——
//! 项目根下的 `tests-001/`，不写系统的临时目录。那边是给系统自己用的，
//! 而且写进去的文件没人负责清，跑几次测试就会堆一片。
//!
//! `tests-001/` 是一次性目录，不进版本库，所以新克隆下来的仓库里可能没有，
//! `scratch_dir()` 会顺手建出来。

use std::path::PathBuf;

/// 测试临时目录（项目根下的 `tests-001/`），不存在就建出来。
pub fn scratch_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests-001");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 在测试临时目录下拼一个文件名。
///
/// 各测试要用不同的名字，不然同时跑起来会互相抢同一个文件。
pub fn scratch_path(name: &str) -> PathBuf {
    scratch_dir().join(name)
}
