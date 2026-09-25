//! native 版の Host: serialport + 受信スレッド、ファイルシステム

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};

use null_term_core::channel::{self, Flow, PortConfig};
use null_term_core::host::{Host, Link, Opened, SerialEvent};
use null_term_core::transfer::{FileSink, Protocol, SendFile};

pub struct NativeHost {
    pub tx: Sender<SerialEvent>,
}

/// `~/` をホームディレクトリに展開する
pub fn expand_path(s: &str) -> PathBuf {
    match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(s),
    }
}

pub fn list_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        // macOS では tty.* と cu.* が対で見える。発信側は cu.* を使う
        .filter(|n| !n.starts_with("/dev/tty."))
        .collect();
    ports.sort();
    ports
}

/// 送るファイルを読み込む
pub fn load_files(paths: &[PathBuf]) -> Result<Vec<SendFile>> {
    paths
        .iter()
        .map(|p| {
            if !p.is_file() {
                bail!("ファイルがありません: {}", p.display());
            }
            let data = std::fs::read(p).with_context(|| format!("読み込めません: {}", p.display()))?;
            let mtime = std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(SendFile { name, data, mtime })
        })
        .collect()
}

/// 受信したファイルをディスクに保存する
pub struct DiskSink {
    /// XMODEM なら保存するファイル、YMODEM なら保存先ディレクトリ
    dest: PathBuf,
    file: Option<File>,
}

impl DiskSink {
    pub fn new(protocol: Protocol, dest: PathBuf) -> Result<Self> {
        match protocol {
            Protocol::Ymodem if !dest.is_dir() => bail!("保存先ディレクトリがありません: {}", dest.display()),
            Protocol::Xmodem | Protocol::Xmodem1k if dest.is_dir() => {
                bail!("XMODEM は保存するファイル名を指定してください: {}", dest.display())
            }
            _ => Ok(DiskSink { dest, file: None }),
        }
    }
}

/// 既存ファイルを上書きしないよう name.1, name.2 ... を付ける
fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    (1..)
        .map(|i| PathBuf::from(format!("{}.{i}", p.display())))
        .find(|c| !c.exists())
        .unwrap()
}

impl FileSink for DiskSink {
    fn create(&mut self, name: Option<&str>) -> Result<String> {
        let path = match name {
            Some(name) => unique_path(&self.dest.join(name)),
            None => self.dest.clone(),
        };
        let f = File::create(&path).with_context(|| format!("保存できません: {}", path.display()))?;
        self.file = Some(f);
        Ok(path.display().to_string())
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.file.as_mut().context("ファイルが開かれていません")?.write_all(data)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(mut f) = self.file.take() {
            f.flush()?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn open_raw(path: &str) -> serialport::Result<Box<dyn SerialPort>> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut port = unsafe { serialport::TTYPort::from_raw_fd(file.into_raw_fd()) };
    port.set_timeout(Duration::from_millis(50))?;
    Ok(Box::new(port))
}

#[cfg(not(unix))]
fn open_raw(path: &str) -> serialport::Result<Box<dyn SerialPort>> {
    Err(serialport::Error::new(serialport::ErrorKind::NoDevice, path))
}

struct NativeLink {
    port: Box<dyn SerialPort>,
    /// 受信スレッドを止める
    stop: Arc<AtomicBool>,
}

impl Drop for NativeLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Link for NativeLink {
    /// 書き込みのタイムアウトは待ち続け、10 秒進まなければエラー
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut rest = bytes;
        let mut last_progress = Instant::now();
        let result = loop {
            if rest.is_empty() {
                break self.port.flush();
            }
            match self.port.write(rest) {
                Ok(n) if n > 0 => {
                    rest = &rest[n..];
                    last_progress = Instant::now();
                }
                Ok(_) => {}
                Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => {}
                Err(e) => break Err(e),
            }
            if last_progress.elapsed() > Duration::from_secs(10) {
                break Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "送信が進みません"));
            }
        };
        result.map_err(|e| e.to_string())
    }

    fn set_baud(&mut self, baud: u32) -> Option<Result<(), String>> {
        Some(self.port.set_baud_rate(baud).map_err(|e| e.to_string()))
    }

    fn pulse_dtr(&mut self) -> Result<(), String> {
        // ctl の hangup が返った時点で回線が切れているよう、戻すまで待つ
        self.port
            .write_data_terminal_ready(false)
            .and_then(|_| {
                thread::sleep(Duration::from_millis(600));
                self.port.write_data_terminal_ready(true)
            })
            .map_err(|e| e.to_string())
    }
}

fn to_serialport(cfg: &PortConfig) -> (DataBits, Parity, StopBits, FlowControl) {
    let data_bits = match cfg.data_bits {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        _ => DataBits::Eight,
    };
    let parity = match cfg.parity {
        channel::Parity::None => Parity::None,
        channel::Parity::Even => Parity::Even,
        channel::Parity::Odd => Parity::Odd,
    };
    let stop_bits = if cfg.stop_bits == 2 { StopBits::Two } else { StopBits::One };
    let flow = match cfg.flow {
        Flow::None => FlowControl::None,
        Flow::Software => FlowControl::Software,
        Flow::Hardware => FlowControl::Hardware,
    };
    (data_bits, parity, stop_bits, flow)
}

impl Host for NativeHost {
    fn open(&self, ch: usize, generation: u64, cfg: &PortConfig) -> Result<Opened, String> {
        let path = cfg.path.clone().unwrap_or_default();
        if path.starts_with("ws://") || path.starts_with("wss://") {
            return Err("WebSocket への接続はブラウザ版だけの機能です".into());
        }
        let (data_bits, parity, stop_bits, flow) = to_serialport(cfg);
        let result = serialport::new(&path, cfg.baud)
            .data_bits(data_bits)
            .parity(parity)
            .stop_bits(stop_bits)
            .flow_control(flow)
            .timeout(Duration::from_millis(50))
            .open();
        // pty など bps 設定を受け付けないデバイスは素の fd として開き直す
        let mut note = "";
        let (port, mut reader) = match result {
            Err(e) => match open_raw(&path) {
                Ok(p) => {
                    note = " (bps 設定なし)";
                    Ok(p)
                }
                Err(_) => Err(e),
            },
            ok => ok,
        }
        .and_then(|p| p.try_clone().map(|r| (p, r)))
        .map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop2.load(Ordering::Relaxed) {
                match reader.read(&mut buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        if tx.send(SerialEvent::Data { ch, generation, data }).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        let _ = tx.send(SerialEvent::Error { ch, generation, msg: e.to_string() });
                        break;
                    }
                }
            }
        });
        Ok(Opened::serial(Box::new(NativeLink { port, stop }), note))
    }

    fn list_ports(&self) -> Vec<String> {
        list_ports()
    }

    fn create_log(&self, ch_name: char) -> Result<(Box<dyn Write>, String)> {
        let path = format!("null-term-{ch_name}-{}.log", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let f = File::create(&path)?;
        Ok((Box::new(f), path))
    }

    fn upload(&self, _ch: usize, _protocol: Protocol, input: &str) -> Result<Option<Vec<SendFile>>> {
        let paths: Vec<PathBuf> = input.split_whitespace().map(expand_path).collect();
        load_files(&paths).map(Some)
    }

    fn download(&self, protocol: Protocol, input: &str) -> Result<Box<dyn FileSink>> {
        Ok(Box::new(DiskSink::new(protocol, expand_path(input))?))
    }
}
