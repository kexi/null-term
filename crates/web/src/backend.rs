//! ratzilla の DomBackend に、正しい画面サイズを返させるラッパー
//!
//! ratzilla 0.3.1 の DomBackend::size() は、採寸したセルの大きさではなく既定値 (10x20px) で
//! 窓の大きさを割った値から 1 を引いて返すため、ratatui のバッファと DOM 上のセル数が食い違う
//! (右端と下端が描かれない)。DomBackend が内部で使うのと同じ計算で size() を返し直す。

use std::cell::Cell as StdCell;
use std::rc::Rc;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratzilla::DomBackend;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::Element;

pub struct FixedDomBackend {
    inner: DomBackend,
    parent: Element,
    /// 採寸したセルの (幅, 高さ) px。窓の大きさが変わったら測り直す
    cell: Rc<StdCell<Option<(f64, f64)>>>,
}

impl FixedDomBackend {
    pub fn new(parent_id: &str) -> Result<Self, String> {
        let inner = DomBackend::new_by_id(parent_id).map_err(|e| e.to_string())?;
        let doc = web_sys::window().unwrap().document().unwrap();
        let parent = doc.get_element_by_id(parent_id).ok_or("描画先の要素がありません")?;
        let cell = Rc::new(StdCell::new(None));
        let reset = cell.clone();
        let on_resize = Closure::<dyn FnMut()>::new(move || reset.set(None));
        let _ = web_sys::window()
            .unwrap()
            .add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
        on_resize.forget();
        Ok(FixedDomBackend { inner, parent, cell })
    }

    /// DomBackend::measure_cell_size と同じ方法で測る
    fn cell_size(&self) -> (f64, f64) {
        if let Some(c) = self.cell.get() {
            return c;
        }
        let doc = web_sys::window().unwrap().document().unwrap();
        let pre = doc.create_element("pre").unwrap();
        let _ = pre.set_attribute("style", "margin: 0; padding: 0; border: 0; line-height: normal;");
        let span = doc.create_element("span").unwrap();
        span.set_inner_html("\u{2588}");
        let _ = span.set_attribute("style", "display: inline-block; width: 1ch;");
        let _ = pre.append_child(&span);
        let _ = self.parent.append_child(&pre);
        let rect = span.get_bounding_client_rect();
        let _ = self.parent.remove_child(&pre);
        let c = if rect.width() > 0.0 && rect.height() > 0.0 { (rect.width(), rect.height()) } else { (10.0, 20.0) };
        self.cell.set(Some(c));
        c
    }
}

impl Backend for FixedDomBackend {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    /// DomBackend::calculate_size と同じ計算 (DOM のセル数と一致させる)
    fn size(&self) -> Result<Size, Self::Error> {
        let (cw, ch) = self.cell_size();
        let rect = self.parent.get_bounding_client_rect();
        Ok(Size::new((rect.width() / cw) as u16, (rect.height() / ch) as u16))
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}
