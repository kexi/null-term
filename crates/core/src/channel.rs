//! 1 チャンネル分のシリアルポート + VT100 画面。
//!
//! ポートの実体は `Host` が作る `Link` で、受信データは `handle_event` で受け取る。

use std::io::Write;

use anyhow::{bail, Context, Result};
use encoding_rs::{Decoder, Encoding, EUC_JP, ISO_2022_JP, SHIFT_JIS, UTF_8};
use web_time::{Duration, Instant};

use crate::host::{Host, Link, SerialEvent};
use crate::transfer::{FileSink, Outcome, Protocol, SendFile, Transfer};

pub const BAUD_RATES: &[u32] = &[
    300, 1200, 2400, 4800, 9600, 14400, 19200, 38400, 57600, 115200, 230400, 460800, 921600,
];

const SCROLLBACK: usize = 5000;

/// ATI3 の応答がモデム名になっていない機種の対応表 (応答, 表示名)
const KNOWN_MODEMS: &[(&str, &str)] = &[("330", "AIWA PV-PF24MK2")];
/// 外部制御の wait 用に保持する受信テキストの上限
const RX_TEXT_MAX: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Newline {
    Cr,
    CrLf,
    Lf,
}

impl Newline {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Newline::Cr => b"\r",
            Newline::CrLf => b"\r\n",
            Newline::Lf => b"\n",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Newline::Cr => "CR",
            Newline::CrLf => "CRLF",
            Newline::Lf => "LF",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Newline::Cr => Newline::CrLf,
            Newline::CrLf => Newline::Lf,
            Newline::Lf => Newline::Cr,
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "cr" => Newline::Cr,
            "crlf" => Newline::CrLf,
            "lf" => Newline::Lf,
            _ => bail!("newline は cr / crlf / lf のいずれか: {s}"),
        })
    }
}

pub const ENCODINGS: &[&Encoding] = &[SHIFT_JIS, UTF_8, EUC_JP, ISO_2022_JP];

pub fn parse_encoding(s: &str) -> Result<&'static Encoding> {
    Ok(match s.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
        "sjis" | "shiftjis" | "cp932" => SHIFT_JIS,
        "utf8" => UTF_8,
        "euc" | "eucjp" => EUC_JP,
        "jis" | "iso2022jp" => ISO_2022_JP,
        _ => bail!("encoding は sjis / utf8 / eucjp / jis のいずれか: {s}"),
    })
}

pub fn encoding_label(enc: &'static Encoding) -> &'static str {
    if enc == SHIFT_JIS {
        "SJIS"
    } else if enc == UTF_8 {
        "UTF-8"
    } else if enc == EUC_JP {
        "EUC-JP"
    } else {
        "JIS"
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parity {
    None,
    Even,
    Odd,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    None,
    /// XON / XOFF
    Software,
    /// RTS / CTS
    Hardware,
}

impl Flow {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "none" | "no" => Flow::None,
            "xon" | "soft" | "sw" => Flow::Software,
            "rts" | "hard" | "hw" => Flow::Hardware,
            _ => bail!("flow は none / xon / rts のいずれか: {s}"),
        })
    }
}

#[derive(Clone, Debug)]
pub struct PortConfig {
    pub path: Option<String>,
    pub baud: u32,
    /// 5〜8
    pub data_bits: u8,
    pub parity: Parity,
    /// 1 または 2
    pub stop_bits: u8,
    pub flow: Flow,
}

impl PortConfig {
    pub fn empty(baud: u32, flow: Flow) -> Self {
        PortConfig { path: None, baud, data_bits: 8, parity: Parity::None, stop_bits: 1, flow }
    }

    /// `PATH[:BAUD[:FMT]]` 形式 (例: `/dev/cu.usbserial-XXXX:9600:8N1`)
    pub fn parse(spec: &str, default_baud: u32, flow: Flow) -> Result<Self> {
        let mut cfg = PortConfig::empty(default_baud, flow);
        let mut parts = spec.split(':');
        let path = parts.next().unwrap_or_default();
        if path.is_empty() {
            bail!("ポートが空です: {spec}");
        }
        cfg.path = Some(path.to_string());
        if let Some(b) = parts.next() {
            cfg.baud = b.parse().with_context(|| format!("bps が不正: {b}"))?;
        }
        if let Some(f) = parts.next() {
            cfg.set_format(f)?;
        }
        if parts.next().is_some() {
            bail!("書式は PATH[:BAUD[:FMT]] です: {spec}");
        }
        Ok(cfg)
    }

    pub fn set_format(&mut self, f: &str) -> Result<()> {
        let b = f.as_bytes();
        if b.len() != 3 {
            bail!("FMT は 8N1 のような 3 文字: {f}");
        }
        self.data_bits = match b[0] {
            b'5'..=b'8' => b[0] - b'0',
            _ => bail!("データビットが不正: {f}"),
        };
        self.parity = match b[1].to_ascii_uppercase() {
            b'N' => Parity::None,
            b'E' => Parity::Even,
            b'O' => Parity::Odd,
            _ => bail!("パリティが不正: {f}"),
        };
        self.stop_bits = match b[2] {
            b'1' | b'2' => b[2] - b'0',
            _ => bail!("ストップビットが不正: {f}"),
        };
        Ok(())
    }

    pub fn format_label(&self) -> String {
        let p = match self.parity {
            Parity::None => 'N',
            Parity::Even => 'E',
            Parity::Odd => 'O',
        };
        let flow = match self.flow {
            Flow::None => "",
            Flow::Software => " XON",
            Flow::Hardware => " RTS",
        };
        format!("{}{p}{}{flow}", self.data_bits, self.stop_bits)
    }
}

pub struct Channel {
    pub index: usize,
    pub cfg: PortConfig,
    pub encoding: &'static Encoding,
    pub newline: Newline,
    pub backspace: u8,
    pub local_echo: bool,
    pub parser: vt100::Parser,
    pub status: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub log_path: Option<String>,
    /// 受信テキスト (デコード済み)。rx_base は rx_text 先頭の通算オフセット
    rx_text: String,
    rx_base: usize,
    /// 外部制御の wait がここから後ろを検索する
    pub mark: usize,
    /// ATI3 で取得したモデム名
    pub modem: Option<String>,
    /// 接続時に ATI3 を送るか
    pub auto_probe: bool,
    /// ATI3 応答待ち (受信テキストの開始オフセット, 期限)
    probe: Option<(usize, Instant)>,
    /// I/O エラー後の自動再接続 (次に試す時刻, 試行回数)
    reconnect: Option<(Instant, u32)>,
    /// 自動再接続で開いている途中 (試行回数)。非同期に開くホストで失敗したら再試行する
    reopening: Option<u32>,
    /// XMODEM / YMODEM 転送中
    pub transfer: Option<Transfer>,
    /// 直前の転送結果 (成功, メッセージ)
    pub transfer_result: Option<(bool, String)>,
    decoder: Decoder,
    writer: Option<Box<dyn Link>>,
    generation: u64,
    log: Option<Box<dyn Write>>,
}

impl Channel {
    pub fn new(index: usize, cfg: PortConfig, encoding: &'static Encoding, newline: Newline, backspace: u8) -> Self {
        Channel {
            index,
            cfg,
            encoding,
            newline,
            backspace,
            local_echo: false,
            parser: vt100::Parser::new(24, 80, SCROLLBACK),
            status: String::new(),
            rx_bytes: 0,
            tx_bytes: 0,
            log_path: None,
            rx_text: String::new(),
            rx_base: 0,
            mark: 0,
            modem: None,
            auto_probe: true,
            probe: None,
            reconnect: None,
            reopening: None,
            transfer: None,
            transfer_result: None,
            decoder: encoding.new_decoder_without_bom_handling(),
            writer: None,
            generation: 0,
            log: None,
        }
    }

    pub fn name(&self) -> char {
        (b'A' + self.index as u8) as char
    }

    pub fn is_open(&self) -> bool {
        self.writer.is_some()
    }

    /// ユーザー操作でポートを開く (自動再接続は解除)
    pub fn open(&mut self, host: &dyn Host) {
        self.reconnect = None;
        self.reopening = None;
        self.open_port(host, self.auto_probe);
    }

    fn open_port(&mut self, host: &dyn Host, probe: bool) {
        self.close_port();
        if self.cfg.path.is_none() {
            self.status = "未接続 (Ctrl-A p でポート選択)".into();
            return;
        }
        // 古い reader からのイベントと区別するため、開くたびに世代を進める
        self.generation += 1;
        let opened = match host.open(self.index, self.generation, &self.cfg) {
            Ok(o) => o,
            Err(e) => {
                self.status = format!("オープン失敗: {e}");
                return;
            }
        };
        self.writer = Some(opened.link);
        self.status = format!("接続中{}", opened.note);
        if probe {
            self.modem = None;
            self.probe_modem();
        }
    }

    /// ユーザー操作でポートを閉じる (自動再接続もしない)
    pub fn close(&mut self) {
        self.reconnect = None;
        self.close_port();
    }

    /// Broken pipe などの I/O エラー: 閉じて自動再接続を予約する
    fn fail(&mut self, msg: String) {
        self.close_port();
        self.reconnect = Some((Instant::now() + Duration::from_secs(1), 0));
        self.status = format!("{msg} → 再接続待ち");
    }

    pub fn is_reconnecting(&self) -> bool {
        self.reconnect.is_some()
    }

    /// 自動再接続の時刻が来ていれば開き直す。状態が変わったら true
    pub fn poll_reconnect(&mut self, host: &dyn Host) -> bool {
        let Some((at, tries)) = self.reconnect else { return false };
        if Instant::now() < at {
            return false;
        }
        // 再接続ではモデム名を問い合わせない (通信中の相手に ATI3 が飛ぶのを避ける)
        self.open_port(host, false);
        if self.is_open() {
            self.reconnect = None;
            self.reopening = Some(tries);
            self.status = format!("自動再接続しました ({} 回目)", tries + 1);
        } else {
            self.retry_reconnect(tries);
        }
        true
    }

    fn retry_reconnect(&mut self, tries: u32) {
        let reason = std::mem::take(&mut self.status);
        self.reconnect = Some((Instant::now() + Duration::from_secs(2), tries + 1));
        self.status = format!("{reason} → 再接続を再試行中 ({} 回目)", tries + 1);
    }

    fn close_port(&mut self) {
        // 古い reader からのイベントを無視するため世代を進める
        self.generation += 1;
        if let Some(t) = self.transfer.take() {
            let msg = format!("{} {}失敗: ポートが閉じられました", t.protocol.label(), t.direction.label());
            self.transfer_result = Some((false, msg));
        }
        if self.writer.take().is_some() {
            self.status = "切断".into();
        }
    }

    pub fn set_baud(&mut self, baud: u32, host: &dyn Host) {
        self.cfg.baud = baud;
        let Some(w) = self.writer.as_mut() else { return };
        match w.set_baud(baud) {
            Some(Ok(())) => {}
            Some(Err(e)) => self.status = format!("bps 変更失敗: {e}"),
            // 開いたまま変えられないポートは開き直す (ATI3 は送らない)
            None => self.open_port(host, false),
        }
    }

    /// DTR を一旦落としてモデムに回線を切らせる (&D2 の場合)
    pub fn hangup(&mut self) {
        let Some(w) = self.writer.as_mut() else { return };
        let result = w.pulse_dtr();
        self.status = match result {
            Ok(()) => "DTR OFF で切断".into(),
            Err(e) => format!("DTR 制御失敗: {e}"),
        };
    }

    pub fn set_encoding(&mut self, enc: &'static Encoding) {
        self.encoding = enc;
        self.decoder = enc.new_decoder_without_bom_handling();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (r, c) = self.parser.screen().size();
        if (r, c) != (rows, cols) && rows > 0 && cols > 0 {
            self.parser.set_size(rows, cols);
        }
    }

    pub fn clear(&mut self) {
        let (r, c) = self.parser.screen().size();
        self.parser = vt100::Parser::new(r, c, SCROLLBACK);
    }

    /// SerialEvent を受け取った時の処理。自チャンネル宛てで世代が一致するものだけ処理する。
    pub fn handle_event(&mut self, ev: SerialEvent) {
        if matches!(ev, SerialEvent::Data { .. }) {
            // 受信できたので、自動再接続で開いたポートは開けている
            self.reopening = None;
        }
        match ev {
            SerialEvent::Data { generation, data, .. } if generation == self.generation => {
                self.rx_bytes += data.len() as u64;
                if let Some(t) = self.transfer.as_mut() {
                    // 転送中の受信データは画面に出さずプロトコルへ渡す
                    let out = t.input(&data, Instant::now());
                    self.write_bytes(&out);
                    self.finish_transfer();
                    return;
                }
                if let Some(log) = self.log.as_mut() {
                    let _ = log.write_all(&data);
                }
                self.feed(&data);
            }
            SerialEvent::Error { generation, msg, .. } if generation == self.generation => {
                self.fail(format!("エラー: {msg}"));
            }
            SerialEvent::OpenFailed { generation, msg, .. } if generation == self.generation => {
                self.close_port();
                self.status = format!("オープン失敗: {msg}");
                if let Some(tries) = self.reopening.take() {
                    self.retry_reconnect(tries);
                }
            }
            _ => {}
        }
    }

    fn feed(&mut self, data: &[u8]) {
        let mut text = String::with_capacity(
            self.decoder.max_utf8_buffer_length(data.len()).unwrap_or(data.len() * 3),
        );
        let _ = self.decoder.decode_to_string(data, &mut text, false);
        self.parser.process(text.as_bytes());
        self.push_rx_text(&text);
        // カーソル位置問い合わせ (DSR 6) に応答
        if text.contains("\x1b[6n") {
            let (row, col) = self.parser.screen().cursor_position();
            let reply = format!("\x1b[{};{}R", row + 1, col + 1);
            self.write_raw(reply.as_bytes());
        }
    }

    /// ATI3 を送ってモデム名を問い合わせる。結果は poll_probe で拾う
    pub fn probe_modem(&mut self) {
        if !self.is_open() {
            return;
        }
        self.probe = Some((self.rx_offset(), Instant::now() + Duration::from_secs(3)));
        self.write_raw(b"ATI3\r");
    }

    /// ATI3 の応答を解析する。状態が変わったら true
    pub fn poll_probe(&mut self) -> bool {
        let Some((from, deadline)) = self.probe else { return false };
        let (_, text) = self.rx_text_since(from);
        let mut lines = Vec::new();
        let mut done = false;
        for line in text.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()) {
            match line {
                "OK" => {
                    done = true;
                    break;
                }
                "ERROR" => {
                    lines.clear();
                    done = true;
                    break;
                }
                l if l.to_ascii_uppercase().starts_with("AT") => {} // エコー
                l => lines.push(l.to_string()),
            }
        }
        if done {
            self.modem = (!lines.is_empty()).then(|| {
                let id = lines.join(" ");
                match KNOWN_MODEMS.iter().find(|(k, _)| *k == id) {
                    Some((_, name)) => name.to_string(),
                    None => id,
                }
            });
            self.probe = None;
            return true;
        }
        if Instant::now() >= deadline {
            self.probe = None;
            return true;
        }
        false
    }

    fn push_rx_text(&mut self, text: &str) {
        self.rx_text.push_str(text);
        if self.rx_text.len() > RX_TEXT_MAX {
            let mut cut = self.rx_text.len() - RX_TEXT_MAX / 2;
            while !self.rx_text.is_char_boundary(cut) {
                cut += 1;
            }
            self.rx_text.drain(..cut);
            self.rx_base += cut;
        }
    }

    /// 受信テキストの通算オフセット (末尾)
    pub fn rx_offset(&self) -> usize {
        self.rx_base + self.rx_text.len()
    }

    /// 通算オフセット `from` 以降の受信テキスト。(実際の開始オフセット, テキスト) を返す
    pub fn rx_text_since(&self, from: usize) -> (usize, &str) {
        let mut i = from.saturating_sub(self.rx_base).min(self.rx_text.len());
        while !self.rx_text.is_char_boundary(i) {
            i += 1;
        }
        (self.rx_base + i, &self.rx_text[i..])
    }

    /// 改行変換なしで、現在のエンコーディングに変換して送信
    pub fn send_encoded(&mut self, s: &str) {
        let (bytes, _, _) = self.encoding.encode(s);
        let bytes = bytes.into_owned();
        self.write_raw(&bytes);
    }

    /// ローカルエコーなしで送信する
    fn write_bytes(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        let Some(w) = self.writer.as_mut() else { return false };
        match w.write(bytes) {
            Ok(()) => {
                self.tx_bytes += bytes.len() as u64;
                true
            }
            Err(e) => {
                self.fail(format!("送信エラー: {e}"));
                false
            }
        }
    }

    pub fn write_raw(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || (self.writer.is_some() && !self.write_bytes(bytes)) {
            return;
        }
        if self.local_echo {
            let mut dec = self.encoding.new_decoder_without_bom_handling();
            let mut text = String::new();
            text.reserve(dec.max_utf8_buffer_length(bytes.len()).unwrap_or(bytes.len() * 3));
            let _ = dec.decode_to_string(bytes, &mut text, true);
            let text = text.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n");
            self.parser.process(text.as_bytes());
        }
    }

    /// 文字列を現在のエンコーディングで送信する (改行は設定に従い変換)
    pub fn send_text(&mut self, s: &str) {
        let nl = std::str::from_utf8(self.newline.bytes()).unwrap();
        let s = s.replace("\r\n", "\n").replace('\r', "\n").replace('\n', nl);
        let (bytes, _, _) = self.encoding.encode(&s);
        let bytes = bytes.into_owned();
        self.write_raw(&bytes);
    }

    /// XMODEM / YMODEM 送信を始める
    pub fn start_upload(&mut self, protocol: Protocol, files: Vec<SendFile>) -> Result<()> {
        self.check_can_transfer()?;
        let t = Transfer::send(protocol, files, Instant::now())?;
        self.status = format!("{} 送信中", protocol.label());
        self.transfer = Some(t);
        Ok(())
    }

    /// XMODEM / YMODEM 受信を始める
    pub fn start_download(&mut self, protocol: Protocol, sink: Box<dyn FileSink>) -> Result<()> {
        self.check_can_transfer()?;
        let (t, out) = Transfer::recv(protocol, sink, Instant::now());
        self.status = format!("{} 受信中", protocol.label());
        self.transfer = Some(t);
        self.write_bytes(&out);
        Ok(())
    }

    fn check_can_transfer(&self) -> Result<()> {
        if !self.is_open() {
            bail!("{} は未接続です", self.name());
        }
        if self.transfer.is_some() {
            bail!("{} は転送中です", self.name());
        }
        Ok(())
    }

    pub fn cancel_transfer(&mut self) {
        if let Some(t) = self.transfer.as_mut() {
            let out = t.cancel();
            self.write_bytes(&out);
            self.finish_transfer();
        }
    }

    /// 転送のタイムアウト処理。状態が変わったら true
    pub fn poll_transfer(&mut self) -> bool {
        let Some(t) = self.transfer.as_mut() else { return false };
        let out = t.tick(Instant::now());
        let changed = !out.is_empty();
        self.write_bytes(&out);
        changed | self.finish_transfer()
    }

    /// 転送が終わっていれば後始末する。終わったら true
    fn finish_transfer(&mut self) -> bool {
        let Some(t) = self.transfer.as_ref() else { return false };
        let result = match &t.outcome {
            Outcome::Running => return false,
            Outcome::Done(msg) => (true, format!("{} {}完了: {msg}", t.protocol.label(), t.direction.label())),
            Outcome::Failed(msg) => (false, format!("{} {}失敗: {msg}", t.protocol.label(), t.direction.label())),
        };
        self.status = result.1.clone();
        self.transfer_result = Some(result);
        self.transfer = None;
        true
    }

    pub fn toggle_log(&mut self, host: &dyn Host) {
        if let Some(mut log) = self.log.take() {
            let _ = log.flush();
            self.status = format!("ログ停止: {}", self.log_path.take().unwrap_or_default());
            return;
        }
        match host.create_log(self.name()) {
            Ok((f, path)) => {
                self.log = Some(f);
                self.status = format!("ログ記録中: {path}");
                self.log_path = Some(path);
            }
            Err(e) => self.status = format!("ログ作成失敗: {e:#}"),
        }
    }

    pub fn is_logging(&self) -> bool {
        self.log.is_some()
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
    }
}
