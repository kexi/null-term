//! null-term: パソコン通信用 2 画面 (上下分割) シリアルターミナル

mod ctl;
mod host;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;

use null_term_core::channel::{self, Channel, Flow, Newline, PortConfig};
use null_term_core::host::SerialEvent;
use null_term_core::keys::{Key, KeyCode};
use null_term_core::{ui, App};

use host::NativeHost;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "パソコン通信用 2 画面シリアルターミナル (上下分割 / USB-UART 2ch)",
    args_conflicts_with_subcommands = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// 外部制御ソケットのパス (既定: $NULL_TERM_SOCK または $TMPDIR/null-term-$USER.sock)
    #[arg(short, long, global = true)]
    socket: Option<PathBuf>,
    /// 画面を出さずに外部制御だけで動かす
    #[arg(long)]
    headless: bool,
    /// 上画面 (A) のポート  PATH[:BAUD[:FMT]]  例: /dev/cu.usbserial-XXXX:9600:8N1
    port_a: Option<String>,
    /// 下画面 (B) のポート  PATH[:BAUD[:FMT]]
    port_b: Option<String>,
    /// bps の既定値 (ポート指定で省略した場合)
    #[arg(short, long, default_value_t = 9600)]
    baud: u32,
    /// 文字コード: sjis / utf8 / eucjp / jis
    #[arg(short, long, default_value = "sjis")]
    encoding: String,
    /// Enter で送る改行: cr / crlf / lf
    #[arg(short, long, default_value = "cr")]
    newline: String,
    /// フロー制御: none / xon / rts
    #[arg(short, long, default_value = "none")]
    flow: String,
    /// BackSpace キーで DEL (0x7F) を送る (既定は BS 0x08)
    #[arg(long)]
    del: bool,
    /// ローカルエコーを有効にして起動
    #[arg(long)]
    echo: bool,
    /// 接続時に ATI3 でモデム名を問い合わせない
    #[arg(long)]
    no_probe: bool,
    /// 利用可能なシリアルポートを一覧表示して終了
    #[arg(short, long)]
    list: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// 起動中の null-term を外部から操作する
    #[command(subcommand)]
    Ctl(ctl::CtlCmd),
}

fn main() -> Result<()> {
    let args = Args::parse();
    let socket = args.socket.clone().unwrap_or_else(ctl::default_socket);
    if let Some(Command::Ctl(cmd)) = args.command {
        let code = ctl::client(&socket, cmd)?;
        std::process::exit(code);
    }
    if args.list {
        for p in host::list_ports() {
            println!("{p}");
        }
        return Ok(());
    }
    let encoding = channel::parse_encoding(&args.encoding)?;
    let newline = Newline::parse(&args.newline)?;
    let flow = Flow::parse(&args.flow)?;
    let bs = if args.del { 0x7f } else { 0x08 };
    let cfg = |spec: &Option<String>| -> Result<PortConfig> {
        match spec {
            Some(s) => PortConfig::parse(s, args.baud, flow),
            None => Ok(PortConfig::empty(args.baud, flow)),
        }
    };
    let (cfg_a, cfg_b) = (cfg(&args.port_a)?, cfg(&args.port_b)?);

    let (tx, rx) = mpsc::channel();
    let mut app = App::new(
        [Channel::new(0, cfg_a, encoding, newline, bs), Channel::new(1, cfg_b, encoding, newline, bs)],
        Box::new(NativeHost { tx }),
    );
    for ch in app.channels.iter_mut() {
        ch.local_echo = args.echo;
        ch.auto_probe = !args.no_probe;
    }
    app.open_all();

    let (ctl_tx, ctl_rx) = mpsc::channel();
    ctl::spawn_server(&socket, ctl_tx)?;

    let result = if args.headless {
        eprintln!("null-term: headless 起動 (制御ソケット {})", socket.display());
        for ch in &app.channels {
            eprintln!("  {}: {} {}", ch.name(), ch.cfg.path.as_deref().unwrap_or("-"), ch.status);
        }
        run(None, &mut app, &rx, &ctl_rx)
    } else {
        let mut terminal = ratatui::init();
        execute!(stdout(), EnableBracketedPaste)?;
        let result = run(Some(&mut terminal), &mut app, &rx, &ctl_rx);
        let _ = execute!(stdout(), DisableBracketedPaste);
        ratatui::restore();
        result
    };
    let _ = std::fs::remove_file(&socket);
    result
}

fn run(
    mut terminal: Option<&mut ratatui::DefaultTerminal>,
    app: &mut App,
    rx: &mpsc::Receiver<SerialEvent>,
    ctl_rx: &mpsc::Receiver<ctl::CtlRequest>,
) -> Result<()> {
    let mut dirty = true;
    let mut pending = Vec::new();
    while !app.quit {
        while let Ok(ev) = rx.try_recv() {
            app.handle_serial(ev);
            dirty = true;
        }
        while let Ok(req) = ctl_rx.try_recv() {
            ctl::handle(app, req, &mut pending);
            dirty = true;
        }
        dirty |= app.poll();
        if !pending.is_empty() {
            ctl::poll_waits(app, &mut pending);
        }
        let Some(terminal) = terminal.as_deref_mut() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        if app.redraw {
            terminal.clear()?;
            app.redraw = false;
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(10))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if let Some(k) = to_key(&k) {
                        app.handle_key(k);
                    }
                }
                Event::Paste(s) => app.paste(&s),
                _ => {}
            }
            dirty = true;
        }
    }
    Ok(())
}

fn to_key(k: &KeyEvent) -> Option<Key> {
    use ratatui::crossterm::event::KeyCode as C;
    let code = match k.code {
        C::Char(c) => KeyCode::Char(c),
        C::Enter => KeyCode::Enter,
        C::Backspace => KeyCode::Backspace,
        C::Tab => KeyCode::Tab,
        C::BackTab => KeyCode::BackTab,
        C::Esc => KeyCode::Esc,
        C::Up => KeyCode::Up,
        C::Down => KeyCode::Down,
        C::Left => KeyCode::Left,
        C::Right => KeyCode::Right,
        C::Home => KeyCode::Home,
        C::End => KeyCode::End,
        C::Insert => KeyCode::Insert,
        C::Delete => KeyCode::Delete,
        C::PageUp => KeyCode::PageUp,
        C::PageDown => KeyCode::PageDown,
        C::F(n) => KeyCode::F(n),
        _ => return None,
    };
    Some(Key {
        code,
        ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
        alt: k.modifiers.contains(KeyModifiers::ALT),
    })
}
