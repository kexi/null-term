//! WebSocket 回線: null-bbs などの BBS に、モデムなしで直接つなぐ
//!
//! null-bbs の WebSocket 回線はバイナリフレームでデータをそのまま流す (telnet の処理はしない) ので、
//! シリアルポートと同じ Link として扱える。

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{ArrayBuffer, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, CloseEvent, MessageEvent, WebSocket};

use null_term_core::host::{Link, SerialEvent};

use crate::{push_event, WebEvent};

pub fn is_ws_url(path: &str) -> bool {
    path.starts_with("ws://") || path.starts_with("wss://")
}

type Handlers = (Closure<dyn FnMut()>, Closure<dyn FnMut(MessageEvent)>, Closure<dyn FnMut(CloseEvent)>);

pub struct WsLink {
    ws: WebSocket,
    /// つながるまでに書かれたデータ。つながったら None
    pending: Rc<RefCell<Option<Vec<u8>>>>,
    /// drop まで生かしておくイベントハンドラ
    _handlers: Handlers,
}

pub fn open(ch: usize, generation: u64, url: &str) -> Result<WsLink, String> {
    let ws = WebSocket::new(url).map_err(|e| format!("{url} を開けません: {e:?}"))?;
    ws.set_binary_type(BinaryType::Arraybuffer);
    let pending = Rc::new(RefCell::new(Some(Vec::new())));
    let opened = Rc::new(RefCell::new(false));

    let (ws2, pending2, opened2) = (ws.clone(), pending.clone(), opened.clone());
    let on_open = Closure::<dyn FnMut()>::new(move || {
        *opened2.borrow_mut() = true;
        if let Some(data) = pending2.borrow_mut().take()
            && !data.is_empty()
        {
            let _ = ws2.send_with_u8_array(&data);
        }
    });
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        let d = e.data();
        let data = match d.dyn_into::<ArrayBuffer>() {
            Ok(buf) => Uint8Array::new(&buf).to_vec(),
            Err(d) => d.as_string().unwrap_or_default().into_bytes(),
        };
        push_event(WebEvent::Serial(SerialEvent::Data { ch, generation, data }));
    });
    let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |e: CloseEvent| {
        let ev = if *opened.borrow() {
            SerialEvent::Closed { ch, generation, msg: "相手が切断しました".into() }
        } else {
            let msg = format!("接続できません (code {}{})", e.code(), if e.reason().is_empty() { String::new() } else { format!(": {}", e.reason()) });
            SerialEvent::OpenFailed { ch, generation, msg }
        };
        push_event(WebEvent::Serial(ev));
    });
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    Ok(WsLink { ws, pending, _handlers: (on_open, on_message, on_close) })
}

impl Link for WsLink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if let Some(p) = self.pending.borrow_mut().as_mut() {
            p.extend_from_slice(bytes);
            return Ok(());
        }
        if self.ws.ready_state() != WebSocket::OPEN {
            return Err("切断されています".into());
        }
        self.ws.send_with_u8_array(bytes).map_err(|e| format!("{e:?}"))
    }

    fn set_baud(&mut self, _baud: u32) -> Option<Result<(), String>> {
        // 回線速度の概念がないので何もしない
        Some(Ok(()))
    }

    /// モデムの回線切断に相当するので、WebSocket を閉じる
    fn pulse_dtr(&mut self) -> Result<(), String> {
        self.ws.close().map_err(|e| format!("{e:?}"))
    }
}

impl Drop for WsLink {
    fn drop(&mut self) {
        // 自分で閉じたときは切断イベントを出さない
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();
    }
}
