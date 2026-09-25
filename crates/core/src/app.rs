//! 2 画面分の状態とキー操作

use crate::channel::{Channel, BAUD_RATES, ENCODINGS};
use crate::host::{Host, SerialEvent};
use crate::keys::{key_to_bytes, Key, KeyCode};
use crate::transfer::{Direction, Protocol};

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Mode {
    Normal,
    /// Ctrl-A を押した直後
    Prefix,
    /// スクロールバック閲覧中
    Scroll,
}

pub enum Popup {
    Baud { sel: usize, custom: String },
    /// `request` はリスト末尾に「新しいポートを許可」を出すか (その行も sel で選べる)
    Port { ports: Vec<String>, sel: usize, request: bool },
    /// XMODEM / YMODEM の送受信ダイアログ
    Transfer { dir: Direction, proto: usize, path: String, error: Option<String> },
    Help,
}

pub struct App {
    pub channels: [Channel; 2],
    pub active: usize,
    pub mode: Mode,
    pub popup: Option<Popup>,
    pub zoom: bool,
    pub scroll: usize,
    pub quit: bool,
    /// 次の描画前に端末を全消去して描き直す
    pub redraw: bool,
    pub host: Box<dyn Host>,
}

impl App {
    pub fn new(channels: [Channel; 2], host: Box<dyn Host>) -> Self {
        App {
            channels,
            active: 0,
            mode: Mode::Normal,
            popup: None,
            zoom: false,
            scroll: 0,
            quit: false,
            redraw: false,
            host,
        }
    }

    /// 両チャンネルのポートを開く
    pub fn open_all(&mut self) {
        for ch in self.channels.iter_mut() {
            ch.open(&*self.host);
        }
    }

    pub fn handle_serial(&mut self, ev: SerialEvent) {
        let ch = ev.ch();
        self.channels[ch].handle_event(ev);
    }

    /// 時間経過で進む処理 (転送のタイムアウト・ATI3 応答・自動再接続)。状態が変わったら true
    pub fn poll(&mut self) -> bool {
        let mut dirty = false;
        for ch in self.channels.iter_mut() {
            dirty |= ch.poll_transfer();
            dirty |= ch.poll_probe();
            dirty |= ch.poll_reconnect(&*self.host);
        }
        dirty
    }

    /// 貼り付けたテキストをアクティブ画面に送る
    pub fn paste(&mut self, s: &str) {
        if self.mode == Mode::Normal && self.popup.is_none() {
            self.channels[self.active].send_text(s);
        }
    }

    pub fn handle_key(&mut self, k: Key) {
        if self.popup.is_some() {
            self.handle_popup_key(k);
            return;
        }
        match self.mode {
            Mode::Normal => {
                if k.is_ctrl('a') {
                    self.mode = Mode::Prefix;
                } else if self.channels[self.active].transfer.is_some() {
                    // 転送中はキー入力を送らない。Esc / Ctrl-X で中止
                    if k.code == KeyCode::Esc || k.is_ctrl('x') {
                        self.channels[self.active].cancel_transfer();
                    }
                } else {
                    let ch = &mut self.channels[self.active];
                    if let Some(bytes) = key_to_bytes(&k, ch) {
                        ch.write_raw(&bytes);
                    }
                }
            }
            Mode::Prefix => {
                self.mode = Mode::Normal;
                self.handle_command(k);
            }
            Mode::Scroll => self.handle_scroll_key(k),
        }
    }

    fn handle_command(&mut self, k: Key) {
        if k.is_ctrl('a') {
            // Ctrl-A Ctrl-A で 0x01 そのものを送る
            self.channels[self.active].write_raw(&[0x01]);
            return;
        }
        if k.is_ctrl('l') {
            self.redraw = true;
            return;
        }
        let host = &*self.host;
        let ch = &mut self.channels[self.active];
        match k.code {
            KeyCode::Tab | KeyCode::Char('o') | KeyCode::Up | KeyCode::Down => self.active ^= 1,
            KeyCode::Char('1') => self.active = 0,
            KeyCode::Char('2') => self.active = 1,
            KeyCode::Char('b') => {
                let sel = BAUD_RATES.iter().position(|&b| b == ch.cfg.baud).unwrap_or(4);
                self.popup = Some(Popup::Baud { sel, custom: String::new() });
            }
            KeyCode::Char('p') => {
                let ports = host.list_ports();
                let sel = ch
                    .cfg
                    .path
                    .as_ref()
                    .and_then(|p| ports.iter().position(|x| x == p))
                    .unwrap_or(0);
                self.popup = Some(Popup::Port { ports, sel, request: host.can_request_port() });
            }
            KeyCode::Char('e') => {
                let i = ENCODINGS.iter().position(|&e| e == ch.encoding).unwrap_or(0);
                ch.set_encoding(ENCODINGS[(i + 1) % ENCODINGS.len()]);
            }
            KeyCode::Char('n') => ch.newline = ch.newline.next(),
            KeyCode::Char('l') => ch.local_echo = !ch.local_echo,
            KeyCode::Char('h') => ch.backspace = if ch.backspace == 0x08 { 0x7f } else { 0x08 },
            KeyCode::Char('c') => ch.clear(),
            KeyCode::Char('r') => ch.open(host),
            KeyCode::Char('i') => ch.probe_modem(),
            KeyCode::Char(c @ ('u' | 'd')) => {
                let dir = if c == 'u' { Direction::Send } else { Direction::Recv };
                let proto = Protocol::ALL.iter().position(|&p| p == Protocol::Ymodem).unwrap();
                let path = if dir == Direction::Send { String::new() } else { host.default_download(Protocol::Ymodem) };
                self.popup = Some(Popup::Transfer { dir, proto, path, error: None });
            }
            KeyCode::Char('H') => ch.hangup(),
            KeyCode::Char('x') => ch.close(),
            KeyCode::Char('L') => ch.toggle_log(host),
            KeyCode::Char('z') => self.zoom = !self.zoom,
            KeyCode::Char('[') | KeyCode::PageUp => {
                self.mode = Mode::Scroll;
                self.scroll = 0;
                if k.code == KeyCode::PageUp {
                    self.handle_scroll_key(k);
                }
            }
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    fn handle_scroll_key(&mut self, k: Key) {
        let ch = &mut self.channels[self.active];
        let page = ch.parser.screen().size().0 as usize;
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll += 1,
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll += page,
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = usize::MAX,
            KeyCode::End | KeyCode::Char('G') => self.scroll = 0,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.scroll = 0,
            _ => {}
        }
        ch.parser.set_scrollback(self.scroll);
        // 実際にスクロールできた量に丸める
        self.scroll = ch.parser.screen().scrollback();
        if matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
            self.mode = Mode::Normal;
        }
    }

    fn handle_popup_key(&mut self, k: Key) {
        let host = &*self.host;
        let index = self.active;
        let ch = &mut self.channels[index];
        let Some(popup) = self.popup.as_mut() else { return };
        let mut close = matches!(k.code, KeyCode::Esc);
        match popup {
            Popup::Help => close = true,
            Popup::Transfer { dir, proto, path, error } => match k.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                    let old = Protocol::ALL[*proto];
                    let n = Protocol::ALL.len();
                    *proto = if matches!(k.code, KeyCode::Left | KeyCode::Up) { (*proto + n - 1) % n } else { (*proto + 1) % n };
                    // 受信先が既定値のままなら新しいプロトコル用の既定値にする
                    if *dir == Direction::Recv && *path == host.default_download(old) {
                        *path = host.default_download(Protocol::ALL[*proto]);
                    }
                    *error = None;
                }
                KeyCode::Char(c) => {
                    path.push(c);
                    *error = None;
                }
                KeyCode::Backspace => {
                    path.pop();
                    *error = None;
                }
                KeyCode::Enter => {
                    let p = Protocol::ALL[*proto];
                    let result = match dir {
                        Direction::Send => match host.upload(index, p, path) {
                            Ok(Some(files)) => ch.start_upload(p, files),
                            // ファイル選択の後でホストが始める
                            Ok(None) => Ok(()),
                            Err(e) => Err(e),
                        },
                        Direction::Recv => host.download(p, path.trim()).and_then(|sink| ch.start_download(p, sink)),
                    };
                    match result {
                        Ok(()) => close = true,
                        Err(e) => *error = Some(format!("{e:#}")),
                    }
                }
                _ => {}
            },
            Popup::Baud { sel, custom } => match k.code {
                KeyCode::Up => *sel = sel.saturating_sub(1),
                KeyCode::Down => *sel = (*sel + 1).min(BAUD_RATES.len() - 1),
                KeyCode::Char(c) if c.is_ascii_digit() && custom.len() < 8 => custom.push(c),
                KeyCode::Backspace => {
                    custom.pop();
                }
                KeyCode::Enter => {
                    let baud = custom.parse::<u32>().ok().filter(|&b| b > 0).unwrap_or(BAUD_RATES[*sel]);
                    ch.set_baud(baud, host);
                    close = true;
                }
                _ => {}
            },
            Popup::Port { ports, sel, request } => {
                let rows = ports.len() + usize::from(*request);
                match k.code {
                    KeyCode::Up => *sel = sel.saturating_sub(1),
                    KeyCode::Down => *sel = (*sel + 1).min(rows.saturating_sub(1)),
                    KeyCode::Enter => {
                        if let Some(p) = ports.get(*sel) {
                            ch.cfg.path = Some(p.clone());
                            ch.open(host);
                        } else if *request {
                            host.request_port(index);
                        }
                        close = true;
                    }
                    _ => {}
                }
            }
        }
        if close {
            self.popup = None;
        }
    }
}
