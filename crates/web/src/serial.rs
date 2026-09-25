//! Web Serial API でのシリアルポート
//!
//! Web Serial は open / read / write がすべて Promise なので、`Link` はすぐに返し、
//! 実際の処理は spawn_local したタスクで行う。結果は `SerialEvent` として App に届ける。

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{EventTarget, ReadableStream, ReadableStreamDefaultReader, WritableStream, WritableStreamDefaultWriter};

use null_term_core::channel::{Flow, Parity, PortConfig};
use null_term_core::host::{Link, SerialEvent};

use crate::{push_event, sleep, WebEvent};

// web-sys の Serial は unstable cfg が要るので (Cargo.toml 参照)、使う分だけ宣言する
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = EventTarget)]
    pub type Serial;
    #[wasm_bindgen(method, js_name = getPorts)]
    fn get_ports(this: &Serial) -> Promise;
    #[wasm_bindgen(method, js_name = requestPort)]
    fn request_port(this: &Serial) -> Promise;

    #[wasm_bindgen(extends = EventTarget)]
    #[derive(Clone)]
    pub type SerialPort;
    #[wasm_bindgen(method)]
    fn open(this: &SerialPort, options: &Object) -> Promise;
    #[wasm_bindgen(method)]
    fn close(this: &SerialPort) -> Promise;
    /// 閉じているか致命的なエラーの後は null
    #[wasm_bindgen(method, getter)]
    fn readable(this: &SerialPort) -> Option<ReadableStream>;
    #[wasm_bindgen(method, getter)]
    fn writable(this: &SerialPort) -> WritableStream;
    #[wasm_bindgen(method, js_name = getInfo)]
    fn get_info(this: &SerialPort) -> Object;
    #[wasm_bindgen(method, js_name = setSignals)]
    fn set_signals(this: &SerialPort, signals: &Object) -> Promise;
}

/// `{ key: value, ... }` を作る
fn object(fields: &[(&str, JsValue)]) -> Object {
    let o = Object::new();
    for (k, v) in fields {
        let _ = Reflect::set(&o, &(*k).into(), v);
    }
    o
}

fn get(o: &JsValue, key: &str) -> JsValue {
    Reflect::get(o, &key.into()).unwrap_or(JsValue::UNDEFINED)
}

thread_local! {
    /// 許可済みのポート (表示名, ポート)。表示名が Channel の path になる
    static PORTS: RefCell<Vec<(String, SerialPort)>> = const { RefCell::new(Vec::new()) };
}

pub fn serial() -> Serial {
    get(&web_sys::window().unwrap().navigator(), "serial").unchecked_into()
}

pub fn is_supported() -> bool {
    let nav = web_sys::window().unwrap().navigator();
    Reflect::has(&nav, &"serial".into()).unwrap_or(false)
}

/// 許可済みポートの表示名一覧
pub fn port_names() -> Vec<String> {
    PORTS.with(|p| p.borrow().iter().map(|(n, _)| n.clone()).collect())
}

fn find_port(name: &str) -> Option<SerialPort> {
    PORTS.with(|p| p.borrow().iter().find(|(n, _)| n == name).map(|(_, port)| port.clone()))
}

/// ポートを登録して表示名を返す。登録済みならその表示名
///
/// 表示名は USB の VID:PID から作る。同じ型番が複数あれば #2, #3 … を付け、
/// 抜き差ししても他のポートの名前がずれないよう、空いている番号を使う。
pub fn register(port: SerialPort) -> String {
    PORTS.with(|p| {
        let mut ports = p.borrow_mut();
        if let Some((n, _)) = ports.iter().find(|(_, q)| Object::is(q, &port)) {
            return n.clone();
        }
        let info = port.get_info();
        let id = |k: &str| get(&info, k).as_f64().map(|v| v as u16);
        let base = match (id("usbVendorId"), id("usbProductId")) {
            (Some(v), Some(pid)) => format!("USB {v:04x}:{pid:04x}"),
            _ => "シリアルポート".to_string(),
        };
        let name = (1..)
            .map(|i| if i == 1 { base.clone() } else { format!("{base} #{i}") })
            .find(|n| ports.iter().all(|(m, _)| m != n))
            .unwrap();
        ports.push((name.clone(), port));
        name
    })
}

pub fn unregister(port: &SerialPort) {
    PORTS.with(|p| p.borrow_mut().retain(|(_, q)| !Object::is(q, port)));
}

/// 過去に許可したポートを登録する
pub async fn load_granted_ports() {
    if let Ok(ports) = JsFuture::from(serial().get_ports()).await {
        for port in js_sys::Array::from(&ports).iter() {
            register(port.unchecked_into());
        }
    }
}

/// ポート選択ダイアログを出す (キー操作の中から呼ぶこと。ユーザー操作がないと拒否される)
pub fn request_port(ch: usize) {
    let promise = serial().request_port();
    spawn_local(async move {
        // Err はダイアログがキャンセルされた
        if let Ok(port) = JsFuture::from(promise).await {
            let name = register(port.unchecked_into());
            push_event(WebEvent::PortGranted { ch, name });
        }
    });
}

fn js_error(e: &JsValue) -> String {
    let get = |k: &str| Reflect::get(e, &k.into()).ok().and_then(|v| v.as_string());
    match (get("name"), get("message")) {
        (Some(n), Some(m)) => format!("{n}: {m}"),
        (_, Some(m)) => m,
        _ => format!("{e:?}"),
    }
}

fn error_name(e: &JsValue) -> String {
    Reflect::get(e, &"name".into()).ok().and_then(|v| v.as_string()).unwrap_or_default()
}

#[derive(Default)]
struct LinkState {
    writer: Option<WritableStreamDefaultWriter>,
    reader: Option<ReadableStreamDefaultReader>,
    /// 開き終わるまでに書かれたデータ
    pending: Vec<u8>,
    closed: bool,
}

pub struct WebLink {
    port: SerialPort,
    state: Rc<RefCell<LinkState>>,
    ch: usize,
    generation: u64,
}

pub fn open(ch: usize, generation: u64, cfg: &PortConfig) -> Result<WebLink, String> {
    let name = cfg.path.as_deref().unwrap_or_default();
    let port = find_port(name).ok_or_else(|| format!("{name} が見つかりません (Ctrl-A p で許可してください)"))?;
    let parity = match cfg.parity {
        Parity::None => "none",
        Parity::Even => "even",
        Parity::Odd => "odd",
    };
    let flow = match cfg.flow {
        Flow::Hardware => "hardware",
        // Web Serial に XON/XOFF はない
        Flow::Software => return Err("ブラウザ版は XON/XOFF フロー制御に対応していません".into()),
        Flow::None => "none",
    };
    let opts = object(&[
        ("baudRate", cfg.baud.into()),
        ("dataBits", cfg.data_bits.into()),
        ("stopBits", cfg.stop_bits.into()),
        ("parity", parity.into()),
        ("flowControl", flow.into()),
        // 受信バッファ (既定 255 バイト) だと描画中に溢れることがある
        ("bufferSize", (64 * 1024).into()),
    ]);
    let state = Rc::new(RefCell::new(LinkState::default()));
    spawn_local(run(port.clone(), opts, state.clone(), ch, generation));
    Ok(WebLink { port, state, ch, generation })
}

async fn run(port: SerialPort, opts: Object, state: Rc<RefCell<LinkState>>, ch: usize, generation: u64) {
    // 直前に閉じたポートは close が終わるまで開けない (InvalidStateError) ので少し待って再試行する
    let mut tries = 0;
    loop {
        match JsFuture::from(port.open(&opts)).await {
            Ok(_) => break,
            Err(e) if error_name(&e) == "InvalidStateError" && tries < 20 => {
                tries += 1;
                sleep(100).await;
            }
            Err(e) => {
                push_event(WebEvent::Serial(SerialEvent::OpenFailed { ch, generation, msg: js_error(&e) }));
                return;
            }
        }
    }
    if state.borrow().closed {
        // 開いている間に Link が捨てられた
        let _ = JsFuture::from(port.close()).await;
        return;
    }
    let writer = match WritableStreamDefaultWriter::new(&port.writable()) {
        Ok(w) => w,
        Err(e) => {
            push_event(WebEvent::Serial(SerialEvent::Error { ch, generation, msg: js_error(&e) }));
            return;
        }
    };
    let pending = std::mem::take(&mut state.borrow_mut().pending);
    write_chunk(&writer, &pending, ch, generation);
    state.borrow_mut().writer = Some(writer);
    read_loop(&port, &state, ch, generation).await;
}

async fn read_loop(port: &SerialPort, state: &Rc<RefCell<LinkState>>, ch: usize, generation: u64) {
    // パリティエラーなどの致命的でないエラーでは readable が作り直されるので読み直す
    while !state.borrow().closed {
        let Some(readable) = port.readable() else { break };
        let Ok(reader) = ReadableStreamDefaultReader::new(&readable) else { break };
        state.borrow_mut().reader = Some(reader.clone());
        loop {
            match JsFuture::from(reader.read()).await {
                Ok(r) => {
                    if get(&r, "done").as_bool().unwrap_or(false) {
                        break;
                    }
                    let data = Uint8Array::new(&get(&r, "value")).to_vec();
                    push_event(WebEvent::Serial(SerialEvent::Data { ch, generation, data }));
                }
                Err(e) => {
                    let fatal = !matches!(
                        error_name(&e).as_str(),
                        "BreakError" | "FramingError" | "ParityError" | "BufferOverrunError"
                    );
                    if fatal && !state.borrow().closed {
                        push_event(WebEvent::Serial(SerialEvent::Error { ch, generation, msg: js_error(&e) }));
                        reader.release_lock();
                        return;
                    }
                    break;
                }
            }
        }
        reader.release_lock();
    }
}

fn write_chunk(writer: &WritableStreamDefaultWriter, bytes: &[u8], ch: usize, generation: u64) {
    if bytes.is_empty() {
        return;
    }
    let promise = writer.write_with_chunk(&Uint8Array::from(bytes));
    spawn_local(async move {
        if let Err(e) = JsFuture::from(promise).await {
            push_event(WebEvent::Serial(SerialEvent::Error { ch, generation, msg: js_error(&e) }));
        }
    });
}

impl Link for WebLink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut st = self.state.borrow_mut();
        match &st.writer {
            Some(w) => write_chunk(w, bytes, self.ch, self.generation),
            None => st.pending.extend_from_slice(bytes),
        }
        Ok(())
    }

    fn set_baud(&mut self, _baud: u32) -> Option<Result<(), String>> {
        // Web Serial は開いたまま bps を変えられない
        None
    }

    fn pulse_dtr(&mut self) -> Result<(), String> {
        let port = self.port.clone();
        let (ch, generation) = (self.ch, self.generation);
        spawn_local(async move {
            let dtr = |on: bool| JsFuture::from(port.set_signals(&object(&[("dataTerminalReady", on.into())])));
            let mut result = dtr(false).await;
            if result.is_ok() {
                sleep(600).await;
                result = dtr(true).await;
            }
            if let Err(e) = result {
                push_event(WebEvent::Status { ch, generation, msg: format!("DTR 制御失敗: {}", js_error(&e)) });
            }
        });
        Ok(())
    }
}

impl Drop for WebLink {
    fn drop(&mut self) {
        let (reader, writer) = {
            let mut st = self.state.borrow_mut();
            st.closed = true;
            (st.reader.take(), st.writer.take())
        };
        let port = self.port.clone();
        spawn_local(async move {
            if let Some(r) = reader {
                let _ = JsFuture::from(r.cancel()).await;
                r.release_lock();
            }
            if let Some(w) = writer {
                // 書きかけのデータを送り切ってから閉じる
                let _ = JsFuture::from(w.close()).await;
                w.release_lock();
            }
            let _ = JsFuture::from(port.close()).await;
        });
    }
}
