//! null-term の native / ブラウザ共通部分
//!
//! 画面 (ratatui)・VT100 解釈・キー操作・XMODEM / YMODEM を持ち、
//! シリアルポートとファイルは `host::Host` 越しに扱う。

pub mod app;
pub mod channel;
pub mod host;
pub mod keys;
pub mod transfer;
pub mod ui;

pub use app::{App, Mode, Popup};
