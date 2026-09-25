//! null-term のブラウザ版 (Web Serial API + ratzilla)
//!
//! 画面・キー操作・転送は null-term-core をそのまま使い、ポートとファイルだけを差し替える。
//! ブラウザはシングルスレッドなので App は thread_local に置き、
//! 非同期タスクからの出来事は WebEvent のキューを経由して渡す。

mod backend;
mod files;
mod serial;
mod ws;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Write;
use std::rc::Rc;

use anyhow::Result;
use ratatui::Terminal;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{ClipboardEvent, CompositionEvent, HtmlTextAreaElement, KeyboardEvent};

use null_term_core::channel::{self, Channel, Flow, Newline, PortConfig};
use null_term_core::host::{Host, Opened, SerialEvent};
use null_term_core::keys::{Key, KeyCode};
use null_term_core::transfer::{FileSink, Protocol, SendFile};
use null_term_core::{ui, App};

/// 未接続の画面に出す案内 (A, B)
const WELCOME: [&str; 2] = [
    "\r\n  null-term ブラウザ版へようこそ\r\n\r\n\
     \x20   Ctrl-A p   ポートを選んで接続 (USB のシリアル機器、または ws:// で BBS)\r\n\
     \x20   Ctrl-A ?   キー操作の一覧\r\n\
     \x20   Ctrl-A z   この画面を最大化 (1 画面で使う)\r\n\r\n\
     \x20 上下は独立した 2 台の端末です。詳しくは右の「使い方」を見てください。\r\n",
    "\r\n  下の画面 (B) も別の回線につなげます。Ctrl-A 2 で選んでから Ctrl-A p\r\n",
];

/// 非同期タスクから App への出来事
pub enum WebEvent {
    Serial(SerialEvent),
    /// requestPort で新しいポートが許可された
    PortGranted { ch: usize, name: String },
    /// 送信ファイルが選ばれた
    Upload { ch: usize, protocol: Protocol, files: Vec<SendFile> },
    Status { ch: usize, generation: u64, msg: String },
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static EVENTS: RefCell<VecDeque<WebEvent>> = const { RefCell::new(VecDeque::new()) };
}

/// App を借りて処理する。借用中 (イベント処理の中から呼ばれた場合) は None
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

pub fn push_event(ev: WebEvent) {
    EVENTS.with(|q| q.borrow_mut().push_back(ev));
    pump();
}

/// たまった出来事を App に渡し、時間経過の処理も進める
///
/// 描画 (requestAnimationFrame) を待たずに受信のたびに呼ぶ。
/// 背景タブでは描画が止まるが、転送の ACK は返し続けたいため。
fn pump() {
    with_app(|app| {
        while let Some(ev) = EVENTS.with(|q| q.borrow_mut().pop_front()) {
            handle_event(app, ev);
        }
        app.poll();
        // ブラウザのタブは閉じられないので終了は無視する
        app.quit = false;
        app.redraw = false;
    });
}

fn handle_event(app: &mut App, ev: WebEvent) {
    match ev {
        WebEvent::Serial(ev) => app.handle_serial(ev),
        WebEvent::PortGranted { ch, name } => {
            let c = &mut app.channels[ch];
            c.cfg.path = Some(name);
            c.open(&*app.host);
        }
        WebEvent::Upload { ch, protocol, files } => {
            let c = &mut app.channels[ch];
            if let Err(e) = c.start_upload(protocol, files) {
                c.status = format!("{e:#}");
            }
        }
        WebEvent::Status { ch, generation: _, msg } => app.channels[ch].status = msg,
    }
}

pub async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = web_sys::window().unwrap().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn storage_key(ch: usize) -> String {
    format!("null-term.port.{}", (b'A' + ch as u8) as char)
}

/// 前回開いたポートを `PATH:BAUD:FMT` で覚えておき、次回起動時に開く
fn save_port(ch: usize, cfg: &PortConfig) {
    let (Some(s), Some(path)) = (storage(), &cfg.path) else { return };
    let fmt = cfg.format_label();
    let fmt = fmt.split(' ').next().unwrap_or("8N1");
    let _ = s.set_item(&storage_key(ch), &format!("{path}:{}:{fmt}", cfg.baud));
}

fn load_port(ch: usize, default_baud: u32, flow: Flow) -> PortConfig {
    let saved = storage().and_then(|s| s.get_item(&storage_key(ch)).ok().flatten());
    // 表示名は ':' を含む (USB 0403:6001) ので後ろから分ける
    let parsed = saved.and_then(|s| {
        let mut it = s.rsplitn(3, ':');
        let (fmt, baud, path) = (it.next()?, it.next()?, it.next()?);
        let mut cfg = PortConfig::empty(baud.parse().ok()?, flow);
        cfg.set_format(fmt).ok()?;
        cfg.path = Some(path.to_string());
        Some(cfg)
    });
    parsed.unwrap_or_else(|| PortConfig::empty(default_baud, flow))
}

/// 最近つないだ WebSocket の URL (新しい順)
const WS_URLS_KEY: &str = "null-term.ws-urls";
/// null-bbs の WebSocket 回線の既定の待ち受け
const DEFAULT_WS_URL: &str = "ws://127.0.0.1:5657";

fn ws_urls() -> Vec<String> {
    let saved = storage().and_then(|s| s.get_item(WS_URLS_KEY).ok().flatten()).unwrap_or_default();
    let mut urls: Vec<String> = saved.lines().filter(|l| ws::is_ws_url(l)).map(String::from).collect();
    if urls.is_empty() {
        urls.push(DEFAULT_WS_URL.into());
    }
    urls
}

fn remember_ws_url(url: &str) {
    let mut urls = ws_urls();
    urls.retain(|u| u != url);
    urls.insert(0, url.to_string());
    urls.truncate(5);
    if let Some(s) = storage() {
        let _ = s.set_item(WS_URLS_KEY, &urls.join("\n"));
    }
}

struct WebHost;

impl Host for WebHost {
    fn open(&self, ch: usize, generation: u64, cfg: &PortConfig) -> Result<Opened, String> {
        let path = cfg.path.as_deref().unwrap_or_default();
        if ws::is_ws_url(path) {
            let link = ws::open(ch, generation, path)?;
            remember_ws_url(path);
            save_port(ch, cfg);
            // WebSocket の相手は null-bbs (UTF-8) で、モデムではないので ATI3 は送らない
            return Ok(Opened { link: Box::new(link), note: " (WebSocket)", modem: false, encoding: Some(encoding_rs::UTF_8) });
        }
        let link = serial::open(ch, generation, cfg)?;
        save_port(ch, cfg);
        Ok(Opened::serial(Box::new(link), ""))
    }

    fn list_ports(&self) -> Vec<String> {
        let mut ports = serial::port_names();
        ports.extend(ws_urls());
        ports
    }

    fn can_request_port(&self) -> bool {
        true
    }

    fn request_port(&self, ch: usize) {
        serial::request_port(ch);
    }

    fn create_log(&self, ch_name: char) -> Result<(Box<dyn Write>, String)> {
        let d = js_sys::Date::new_0();
        let name = format!(
            "null-term-{ch_name}-{:04}{:02}{:02}-{:02}{:02}{:02}.log",
            d.get_full_year(),
            d.get_month() + 1,
            d.get_date(),
            d.get_hours(),
            d.get_minutes(),
            d.get_seconds()
        );
        let label = format!("{name} (停止でダウンロード)");
        Ok((Box::new(files::LogFile::new(name)), label))
    }

    fn upload(&self, ch: usize, protocol: Protocol, _input: &str) -> Result<Option<Vec<SendFile>>> {
        files::pick_upload(ch, protocol);
        Ok(None)
    }

    fn download(&self, _protocol: Protocol, input: &str) -> Result<Box<dyn FileSink>> {
        Ok(Box::new(files::DownloadSink::new(input)))
    }

    fn picks_files(&self) -> bool {
        true
    }

    fn can_quit(&self) -> bool {
        false
    }

    fn default_download(&self, protocol: Protocol) -> String {
        if protocol == Protocol::Ymodem { String::new() } else { "download.bin".into() }
    }
}

/// KeyboardEvent を core のキーにする。ブラウザに任せるキーは None
fn to_key(e: &KeyboardEvent) -> Option<Key> {
    // Cmd (⌘) の組み合わせと F12 (開発者ツール) はブラウザに任せる
    if e.meta_key() || e.is_composing() {
        return None;
    }
    let key = e.key();
    let mut chars = key.chars();
    let code = match (chars.next(), chars.next()) {
        (Some(c), None) => KeyCode::Char(c),
        _ => match key.as_str() {
            "Enter" => KeyCode::Enter,
            "Backspace" => KeyCode::Backspace,
            "Tab" if e.shift_key() => KeyCode::BackTab,
            "Tab" => KeyCode::Tab,
            "Escape" => KeyCode::Esc,
            "ArrowUp" => KeyCode::Up,
            "ArrowDown" => KeyCode::Down,
            "ArrowLeft" => KeyCode::Left,
            "ArrowRight" => KeyCode::Right,
            "Home" => KeyCode::Home,
            "End" => KeyCode::End,
            "Insert" => KeyCode::Insert,
            "Delete" => KeyCode::Delete,
            "PageUp" => KeyCode::PageUp,
            "PageDown" => KeyCode::PageDown,
            f if f.starts_with('F') => match f[1..].parse::<u8>() {
                Ok(n @ 1..=11) => KeyCode::F(n),
                _ => return None,
            },
            _ => return None,
        },
    };
    Some(Key { code, ctrl: e.ctrl_key(), alt: e.alt_key() })
}

/// キー入力・IME・貼り付けを受ける textarea を用意する
///
/// window で keydown を拾うだけだと IME が使えないので、見えない textarea に
/// フォーカスを置き、確定した文字列 (compositionend) をそのまま送る。
fn setup_input() {
    let doc = web_sys::window().unwrap().document().unwrap();
    let ta: HtmlTextAreaElement = doc.create_element("textarea").unwrap().unchecked_into();
    ta.set_id("input");
    let _ = ta.set_attribute("autocomplete", "off");
    let _ = ta.set_attribute("autocapitalize", "off");
    let _ = ta.set_attribute("spellcheck", "false");
    let _ = doc.body().unwrap().append_child(&ta);
    let _ = ta.focus();

    let on_key = Closure::<dyn FnMut(KeyboardEvent)>::new(|e: KeyboardEvent| {
        let Some(k) = to_key(&e) else { return };
        // Ctrl-A (全選択) や Tab (フォーカス移動) をブラウザに渡さない
        e.prevent_default();
        with_app(|app| app.handle_key(k));
        pump();
    });
    let _ = ta.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());
    on_key.forget();

    let target = ta.clone();
    let on_compose = Closure::<dyn FnMut(CompositionEvent)>::new(move |e: CompositionEvent| {
        if let Some(text) = e.data() {
            with_app(|app| app.paste(&text));
        }
        target.set_value("");
    });
    let _ = ta.add_event_listener_with_callback("compositionend", on_compose.as_ref().unchecked_ref());
    on_compose.forget();

    // keydown を経ずに挿入された文字 (モバイルのキーボード、音声入力、自動操作など) も送る。
    // keydown で処理したキーは preventDefault しているので、ここには来ない
    let target = ta.clone();
    let on_input = Closure::<dyn FnMut(web_sys::InputEvent)>::new(move |e: web_sys::InputEvent| {
        if e.is_composing() {
            return;
        }
        let text = target.value();
        target.set_value("");
        if !text.is_empty() {
            with_app(|app| app.paste(&text));
        }
    });
    let _ = ta.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref());
    on_input.forget();

    let on_paste = Closure::<dyn FnMut(ClipboardEvent)>::new(|e: ClipboardEvent| {
        e.prevent_default();
        let text = e.clipboard_data().and_then(|d| d.get_data("text").ok()).unwrap_or_default();
        with_app(|app| app.paste(&text));
    });
    let _ = ta.add_event_listener_with_callback("paste", on_paste.as_ref().unchecked_ref());
    on_paste.forget();

    // どこをクリックしても入力を受けられるようにする
    let target = ta.clone();
    let refocus = Closure::<dyn FnMut()>::new(move || {
        let _ = target.focus();
    });
    let _ = doc.add_event_listener_with_callback("click", refocus.as_ref().unchecked_ref());
    refocus.forget();
}

/// USB を抜き差ししたポートを登録簿に反映する
fn watch_ports() {
    let on_connect = Closure::<dyn FnMut(web_sys::Event)>::new(|e: web_sys::Event| {
        if let Some(port) = e.target() {
            serial::register(port.unchecked_into());
        }
    });
    let on_disconnect = Closure::<dyn FnMut(web_sys::Event)>::new(|e: web_sys::Event| {
        if let Some(port) = e.target() {
            serial::unregister(port.unchecked_ref());
        }
    });
    let s = serial::serial();
    let _ = s.add_event_listener_with_callback("connect", on_connect.as_ref().unchecked_ref());
    let _ = s.add_event_listener_with_callback("disconnect", on_disconnect.as_ref().unchecked_ref());
    on_connect.forget();
    on_disconnect.forget();
}

/// URL の ?baud=2400&enc=sjis&newline=cr&flow=rts&del&echo&noprobe で既定値を変える。
/// ?a=ws://… / ?b=ws://… で、その画面を WebSocket (BBS) につないで起動する
struct Options {
    ports: [Option<String>; 2],
    baud: u32,
    encoding: &'static encoding_rs::Encoding,
    newline: Newline,
    flow: Flow,
    del: bool,
    echo: bool,
    probe: bool,
}

fn options() -> Result<Options> {
    let search = web_sys::window().unwrap().location().search().unwrap_or_default();
    let q = web_sys::UrlSearchParams::new_with_str(&search).map_err(|_| anyhow::anyhow!("URL が不正です"))?;
    let get = |k: &str| q.get(k);
    Ok(Options {
        ports: [get("a"), get("b")],
        baud: get("baud").map(|b| b.parse()).transpose()?.unwrap_or(9600),
        encoding: channel::parse_encoding(&get("enc").unwrap_or_else(|| "sjis".into()))?,
        newline: Newline::parse(&get("newline").unwrap_or_else(|| "cr".into()))?,
        flow: Flow::parse(&get("flow").unwrap_or_else(|| "none".into()))?,
        del: q.has("del"),
        echo: q.has("echo"),
        probe: !q.has("noprobe"),
    })
}

fn show_message(msg: &str) {
    let doc = web_sys::window().unwrap().document().unwrap();
    let p = doc.create_element("p").unwrap();
    p.set_class_name("message");
    p.set_text_content(Some(msg));
    let body = doc.body().unwrap();
    let _ = body.insert_before(&p, body.first_child().as_ref());
}

fn main() {
    console_error_panic_hook::set_once();
    if !serial::is_supported() {
        show_message(
            "このブラウザは Web Serial API に対応していません。Chrome / Edge などの Chromium 系ブラウザで開いてください。",
        );
        return;
    }
    if let Err(e) = start() {
        show_message(&format!("起動できません: {e:#}"));
    }
}

fn start() -> Result<()> {
    let o = options()?;
    let bs = if o.del { 0x7f } else { 0x08 };
    let channels = [0, 1].map(|i| {
        let mut cfg = load_port(i, o.baud, o.flow);
        if let Some(p) = &o.ports[i] {
            cfg.path = Some(p.clone());
        }
        let mut ch = Channel::new(i, cfg, o.encoding, o.newline, bs);
        ch.local_echo = o.echo;
        ch.auto_probe = o.probe;
        ch.status = "未接続 (Ctrl-A p でポート選択)".into();
        if ch.cfg.path.is_none() {
            ch.parser.process(WELCOME[i].as_bytes());
        }
        ch
    });
    APP.with(|a| *a.borrow_mut() = Some(App::new(channels, Box::new(WebHost))));

    setup_input();
    watch_ports();
    spawn_local(async {
        serial::load_granted_ports().await;
        // 前回のポート (許可済みのもの) か WebSocket なら開く
        with_app(|app| {
            for ch in app.channels.iter_mut() {
                if ch.cfg.path.as_ref().is_some_and(|p| ws::is_ws_url(p) || serial::port_names().contains(p)) {
                    ch.open(&*app.host);
                }
            }
        });
    });

    let backend = backend::FixedDomBackend::new("term").map_err(|e| anyhow::anyhow!("{e}"))?;
    let terminal = Rc::new(RefCell::new(Terminal::new(backend)?));
    let last_draw = Rc::new(std::cell::Cell::new(0.0));
    let render = {
        let (terminal, last_draw) = (terminal.clone(), last_draw.clone());
        move || {
            pump();
            with_app(|app| {
                let mut terminal = terminal.borrow_mut();
                if terminal.backend().take_resized() {
                    // DOM のセルが作り直されたので差分ではなく全体を描く
                    let _ = terminal.clear();
                }
                let _ = terminal.draw(|f| ui::draw(f, app));
            });
            last_draw.set(js_sys::Date::now());
        }
    };

    // 転送のタイムアウトなど時間で進む処理。
    // 窓が隠れていると requestAnimationFrame が止まるので、しばらく描いていなければここでも描く
    let draw_if_stalled = render.clone();
    let tick = Closure::<dyn FnMut()>::new(move || {
        if js_sys::Date::now() - last_draw.get() > 200.0 {
            draw_if_stalled();
        } else {
            pump();
        }
    });
    let _ = web_sys::window()
        .unwrap()
        .set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 50);
    tick.forget();

    // ratzilla の draw_web は DomBackend 前提なので、requestAnimationFrame のループを自前で回す
    // 自分自身を次のフレームに登録するため、クロージャを共有セルに入れる
    type Frame = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;
    let frame: Frame = Rc::new(RefCell::new(None));
    let next = frame.clone();
    *frame.borrow_mut() = Some(Closure::new(move || {
        render();
        request_frame(next.borrow().as_ref().unwrap());
    }));
    request_frame(frame.borrow().as_ref().unwrap());
    Ok(())
}

fn request_frame(f: &Closure<dyn FnMut()>) {
    let _ = web_sys::window().unwrap().request_animation_frame(f.as_ref().unchecked_ref());
}
