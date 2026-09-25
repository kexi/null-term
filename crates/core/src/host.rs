//! 実行環境 (native CLI / ブラウザ) ごとに差し替える部分
//!
//! core はシリアルポート・ファイル・時刻源を直接触らず、ここの trait 越しに使う。
//! 受信データやエラーはホストが `SerialEvent` として `App::handle_serial` に渡す。

use std::io::Write;

use anyhow::Result;
use encoding_rs::Encoding;

use crate::channel::PortConfig;
use crate::transfer::{FileSink, Protocol, SendFile};

/// ホストから core に渡すシリアルポートの出来事
pub enum SerialEvent {
    Data { ch: usize, generation: u64, data: Vec<u8> },
    /// 開いた後の I/O エラー (自動再接続の対象)
    Error { ch: usize, generation: u64, msg: String },
    /// 非同期に開くホストで、開くのに失敗した
    OpenFailed { ch: usize, generation: u64, msg: String },
    /// 相手から切られた (WebSocket など)。自動再接続はしない
    Closed { ch: usize, generation: u64, msg: String },
}

impl SerialEvent {
    pub fn ch(&self) -> usize {
        match self {
            SerialEvent::Data { ch, .. }
            | SerialEvent::Error { ch, .. }
            | SerialEvent::OpenFailed { ch, .. }
            | SerialEvent::Closed { ch, .. } => *ch,
        }
    }
}

/// 開いているシリアルポート。drop で閉じる
pub trait Link {
    /// 全部書くか、エラーを返す
    fn write(&mut self, bytes: &[u8]) -> Result<(), String>;
    /// 開いたまま bps を変える。変えられないポートは None (開き直す)
    fn set_baud(&mut self, baud: u32) -> Option<Result<(), String>>;
    /// DTR を一旦落として戻す (モデムの回線切断)
    fn pulse_dtr(&mut self) -> Result<(), String>;
}

pub struct Opened {
    pub link: Box<dyn Link>,
    /// 状態表示に添える注記 (例: " (bps 設定なし)")
    pub note: &'static str,
    /// 相手がモデムで、接続時に ATI3 を送ってよい (BBS に直接つながる回線では false)
    pub modem: bool,
    /// 相手の文字コードが決まっていれば、開いたときにそれに切り替える
    pub encoding: Option<&'static Encoding>,
}

impl Opened {
    /// シリアルポート (相手はモデムかもしれない、文字コードは利用者の設定のまま)
    pub fn serial(link: Box<dyn Link>, note: &'static str) -> Self {
        Opened { link, note, modem: true, encoding: None }
    }
}

pub trait Host {
    /// ポートを開き、受信を始める。受信データは `generation` を付けて SerialEvent で返すこと
    fn open(&self, ch: usize, generation: u64, cfg: &PortConfig) -> Result<Opened, String>;
    /// ポート選択ダイアログに出す名前
    fn list_ports(&self) -> Vec<String>;
    /// ポート選択ダイアログに「新しいポートを許可」を出すか (Web Serial の requestPort)
    fn can_request_port(&self) -> bool {
        false
    }
    /// 新しいポートの許可を求める。許可されたらホストが接続まで行う
    fn request_port(&self, _ch: usize) {}
    /// 受信ログの書き込み先を作る。(書き込み先, 表示名)
    fn create_log(&self, ch_name: char) -> Result<(Box<dyn Write>, String)>;
    /// 送信ダイアログで Enter が押された。`input` はダイアログの入力欄。
    /// すぐ送れるなら Some、ファイル選択などで後から送るなら None
    /// (その場合ホストが `Channel::start_upload` を呼ぶ)
    fn upload(&self, ch: usize, protocol: Protocol, input: &str) -> Result<Option<Vec<SendFile>>>;
    /// 受信ダイアログで Enter が押された。保存先を作る
    fn download(&self, protocol: Protocol, input: &str) -> Result<Box<dyn FileSink>>;
    /// ファイルをパス入力ではなくブラウザのダイアログで選ぶ
    fn picks_files(&self) -> bool {
        false
    }
    /// Ctrl-A q で終了できる (ブラウザのタブは閉じられないので false)
    fn can_quit(&self) -> bool {
        true
    }
    /// 受信ダイアログの入力欄の既定値
    fn default_download(&self, protocol: Protocol) -> String {
        if protocol == Protocol::Ymodem { ".".into() } else { "download.bin".into() }
    }
}
