//! ファイルの受け渡し: 送信はファイル選択ダイアログ、受信とログはダウンロード

use std::io::Write;

use anyhow::Result;
use js_sys::{Array, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Blob, BlobPropertyBag, HtmlAnchorElement, HtmlInputElement, Url};

use null_term_core::transfer::{FileSink, Protocol, SendFile};

use crate::{push_event, WebEvent};

/// バイト列をファイルとしてダウンロードさせる
pub fn download(name: &str, data: &[u8]) {
    let parts = Array::new();
    parts.push(&Uint8Array::from(data));
    let opts = BlobPropertyBag::new();
    opts.set_type("application/octet-stream");
    let Ok(blob) = Blob::new_with_u8_array_sequence_and_options(&parts, &opts) else { return };
    let Ok(url) = Url::create_object_url_with_blob(&blob) else { return };
    let doc = web_sys::window().unwrap().document().unwrap();
    let a: HtmlAnchorElement = doc.create_element("a").unwrap().unchecked_into();
    a.set_href(&url);
    a.set_download(name);
    a.click();
    // click 直後に revoke するとダウンロードが始まらないブラウザがある
    let revoke = Closure::once_into_js(move || {
        let _ = Url::revoke_object_url(&url);
    });
    let _ = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000);
}

/// 受信したファイルを 1 ファイルずつダウンロードさせる
pub struct DownloadSink {
    /// XMODEM で保存する名前
    xmodem_name: String,
    current: Option<(String, Vec<u8>)>,
}

impl DownloadSink {
    pub fn new(xmodem_name: &str) -> Self {
        let name = if xmodem_name.is_empty() { "download.bin" } else { xmodem_name };
        DownloadSink { xmodem_name: name.to_string(), current: None }
    }
}

impl FileSink for DownloadSink {
    fn create(&mut self, name: Option<&str>) -> Result<String> {
        let name = name.unwrap_or(&self.xmodem_name).to_string();
        self.current = Some((name.clone(), Vec::new()));
        Ok(name)
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        if let Some((_, buf)) = self.current.as_mut() {
            buf.extend_from_slice(data);
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let Some((name, data)) = self.current.take() {
            download(&name, &data);
        }
        Ok(())
    }
}

/// 受信ログ。止めた (drop した) 時にダウンロードさせる
pub struct LogFile {
    name: String,
    data: Vec<u8>,
}

impl LogFile {
    pub fn new(name: String) -> Self {
        LogFile { name, data: Vec::new() }
    }
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogFile {
    fn drop(&mut self) {
        download(&self.name, &self.data);
    }
}

/// ファイル選択ダイアログを出し、選ばれたら WebEvent::Upload を送る
/// (キー操作の中から呼ぶこと。ユーザー操作がないとダイアログが出ない)
pub fn pick_upload(ch: usize, protocol: Protocol) {
    let doc = web_sys::window().unwrap().document().unwrap();
    let input: HtmlInputElement = doc.create_element("input").unwrap().unchecked_into();
    input.set_type("file");
    input.set_multiple(protocol == Protocol::Ymodem);
    let target = input.clone();
    let on_change = Closure::once_into_js(move || {
        let Some(list) = target.files() else { return };
        let files: Vec<web_sys::File> = (0..list.length()).filter_map(|i| list.get(i)).collect();
        if files.is_empty() {
            return;
        }
        spawn_local(async move {
            let mut out = Vec::new();
            for f in files {
                let Ok(buf) = wasm_bindgen_futures::JsFuture::from(f.array_buffer()).await else { continue };
                out.push(SendFile {
                    name: f.name(),
                    data: Uint8Array::new(&buf).to_vec(),
                    mtime: (f.last_modified() / 1000.0) as u64,
                });
            }
            push_event(WebEvent::Upload { ch, protocol, files: out });
        });
    });
    input.set_onchange(Some(on_change.unchecked_ref()));
    input.click();
}
