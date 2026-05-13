//! Inspector TUI — runs inside a foot popup spawned by `popup::toggle("agt:<id>")`.
//! Top: streaming transcript. Bottom: single-line input box.
//! Keys: Enter sends; Ctrl+C stops session; Ctrl+D exits inspector.
//! Scroll: PgUp/PgDn, Up/Down, Home/End. Sending a prompt snaps to the live tail.

use std::{
    io::{self, Write},
    process::Command,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::Mutex,
};

use crate::agt::{
    protocol::{ClientReply, ClientReq, DaemonEvent, TranscriptEntry},
    socket_path,
};

pub fn run(session_id: &str) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(session_id))
}

async fn run_async(session_id: &str) -> Result<()> {
    let saved = stty_save();
    stty_raw();
    print!("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H"); // alt screen, hide cursor
    io::stdout().flush().ok();

    let result = main_loop(session_id).await;

    print!("\x1b[?25h\x1b[?1049l"); // show cursor, leave alt screen
    io::stdout().flush().ok();
    if let Some(s) = saved.as_deref() {
        stty_restore(s);
    } else {
        let _ = Command::new("stty").arg("sane").status();
    }
    result
}

async fn main_loop(session_id: &str) -> Result<()> {
    let state = Arc::new(Mutex::new(State::default()));

    // Connect twice: one stream for tailing events, one for sending prompts.
    let mut tail_stream = UnixStream::connect(socket_path())
        .await
        .context("connect daemon (tail)")?;
    let req = ClientReq::Tail {
        session_id: session_id.to_string(),
        follow: true,
        replay: true,
    };
    let line = serde_json::to_vec(&req)?;
    tail_stream.write_all(&line).await?;
    tail_stream.write_all(b"\n").await?;
    tail_stream.flush().await?;

    let (tail_rd, _tail_wr) = tail_stream.into_split();
    let event_state = state.clone();
    let id_for_task = session_id.to_string();
    tokio::spawn(async move {
        let mut lines = BufReader::new(tail_rd).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(reply) = serde_json::from_str::<ClientReply>(&line) {
                if let ClientReply::Event { event: e } = reply {
                    let mut s = event_state.lock().await;
                    apply_event(&mut s, e);
                }
            }
        }
        let _ = id_for_task;
    });

    // Render loop: redraw periodically so the latest transcript shows even if
    // user is idle, and react to size changes.
    let render_state = state.clone();
    tokio::spawn(async move {
        loop {
            {
                let s = render_state.lock().await;
                draw(&s);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // Stdin reader: parses bytes (including ANSI escape sequences) into Keys.
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 64];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = stdin.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        let (keys, leftover) = parse_keys(&pending);
        pending = leftover;
        for k in keys {
            match k {
                Key::CtrlD => return Ok(()),
                Key::CtrlC => {
                    let _ = send_stop(session_id).await;
                    return Ok(());
                }
                Key::Enter => {
                    let mut s = state.lock().await;
                    let text = std::mem::take(&mut s.input);
                    s.scroll_offset = 0;
                    drop(s);
                    if !text.trim().is_empty() {
                        let _ = send_prompt(session_id, &text).await;
                    }
                }
                Key::Backspace => {
                    state.lock().await.input.pop();
                }
                Key::CtrlL => {
                    print!("\x1b[2J");
                }
                Key::Char(c) => {
                    state.lock().await.input.push(c);
                }
                Key::ScrollUp(n) => {
                    let body = body_rows();
                    let mut s = state.lock().await;
                    let max = s.transcript.len().saturating_sub(body);
                    s.scroll_offset = s.scroll_offset.saturating_add(n).min(max);
                }
                Key::ScrollDown(n) => {
                    state.lock().await.scroll_offset =
                        state.lock().await.scroll_offset.saturating_sub(n);
                }
                Key::Home => {
                    let body = body_rows();
                    let mut s = state.lock().await;
                    s.scroll_offset = s.transcript.len().saturating_sub(body);
                }
                Key::End => {
                    state.lock().await.scroll_offset = 0;
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
enum Key {
    CtrlC,
    CtrlD,
    CtrlL,
    Enter,
    Backspace,
    Char(char),
    ScrollUp(usize),
    ScrollDown(usize),
    Home,
    End,
}

/// Parse a byte buffer into a sequence of Keys. Returns any unconsumed
/// trailing bytes (an in-flight escape sequence) so the caller can prepend
/// them to the next read.
fn parse_keys(buf: &[u8]) -> (Vec<Key>, Vec<u8>) {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == 0x1b {
            // ESC alone, or ESC [ ... <terminator>
            if i + 1 >= buf.len() {
                return (keys, buf[i..].to_vec());
            }
            if buf[i + 1] != b'[' {
                // Unknown ESC sequence — drop ESC and continue.
                i += 1;
                continue;
            }
            let mut j = i + 2;
            while j < buf.len() {
                let c = buf[j];
                let final_byte = matches!(c, b'A'..=b'Z' | b'a'..=b'z' | b'~');
                if final_byte {
                    break;
                }
                j += 1;
            }
            if j >= buf.len() {
                return (keys, buf[i..].to_vec());
            }
            let seq = &buf[i + 2..=j];
            if let Some(k) = decode_csi(seq) {
                keys.push(k);
            }
            i = j + 1;
            continue;
        }
        match b {
            0x04 => keys.push(Key::CtrlD),
            0x03 => keys.push(Key::CtrlC),
            b'\r' | b'\n' => keys.push(Key::Enter),
            0x7f | 0x08 => keys.push(Key::Backspace),
            0x0c => keys.push(Key::CtrlL),
            c if (32..127).contains(&c) => keys.push(Key::Char(c as char)),
            _ => {}
        }
        i += 1;
    }
    (keys, Vec::new())
}

fn decode_csi(seq: &[u8]) -> Option<Key> {
    // seq is the body after `ESC [`, including the final byte.
    let last = *seq.last()?;
    match last {
        b'A' => Some(Key::ScrollUp(1)),   // Up
        b'B' => Some(Key::ScrollDown(1)), // Down
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'~' => {
            let params = &seq[..seq.len() - 1];
            let head = params.split(|&c| c == b';').next()?;
            match head {
                b"5" => Some(Key::ScrollUp(body_rows().saturating_sub(2).max(1))), // PgUp
                b"6" => Some(Key::ScrollDown(body_rows().saturating_sub(2).max(1))), // PgDn
                b"1" | b"7" => Some(Key::Home),
                b"4" | b"8" => Some(Key::End),
                _ => None,
            }
        }
        _ => None,
    }
}

fn body_rows() -> usize {
    let (rows, _cols) = term_size();
    rows.saturating_sub(3)
}

async fn send_prompt(session_id: &str, text: &str) -> Result<()> {
    let mut s = UnixStream::connect(socket_path()).await?;
    let req = ClientReq::Prompt {
        session_id: session_id.to_string(),
        text: text.trim().to_string(),
    };
    let line = serde_json::to_vec(&req)?;
    s.write_all(&line).await?;
    s.write_all(b"\n").await?;
    s.flush().await?;
    let mut lines = BufReader::new(s).lines();
    let _ = lines.next_line().await;
    Ok(())
}

async fn send_stop(session_id: &str) -> Result<()> {
    let mut s = UnixStream::connect(socket_path()).await?;
    let req = ClientReq::Stop {
        session_id: session_id.to_string(),
    };
    let line = serde_json::to_vec(&req)?;
    s.write_all(&line).await?;
    s.write_all(b"\n").await?;
    s.flush().await?;
    Ok(())
}

#[derive(Default)]
struct State {
    transcript: Vec<String>,
    input: String,
    status: String,
    permission: Option<String>,
    /// Lines from the bottom of the transcript. 0 = live tail.
    scroll_offset: usize,
}

fn apply_event(state: &mut State, event: DaemonEvent) {
    let mut appended = 0usize;
    match event {
        DaemonEvent::Transcript { entry, .. } => {
            for line in render_entry(&entry) {
                state.transcript.push(line);
                appended += 1;
            }
        }
        DaemonEvent::Status { status, .. } => {
            state.status = status.label().to_string();
        }
        DaemonEvent::Permission { summary, .. } => {
            state.permission = Some(summary);
        }
        DaemonEvent::Closed { reason, .. } => {
            state
                .transcript
                .push(format!("\x1b[2m── closed: {reason}\x1b[0m"));
            appended += 1;
        }
    }
    // Keep the user's view stable when scrolled up: bumping the offset by the
    // number of newly appended lines means the same content stays on screen.
    if appended > 0 && state.scroll_offset > 0 {
        state.scroll_offset = state.scroll_offset.saturating_add(appended);
    }
    if state.transcript.len() > 4000 {
        let drop = state.transcript.len() - 4000;
        state.transcript.drain(..drop);
        state.scroll_offset = state.scroll_offset.saturating_sub(drop);
    }
}

fn render_entry(e: &TranscriptEntry) -> Vec<String> {
    match e {
        TranscriptEntry::AgentText { text } => text.lines().map(|l| l.to_string()).collect(),
        TranscriptEntry::UserText { text } => text
            .lines()
            .map(|l| format!("\x1b[36m> {l}\x1b[0m"))
            .collect(),
        TranscriptEntry::ToolCall { tool, .. } => {
            vec![format!("\x1b[33m· tool: {tool}\x1b[0m")]
        }
        TranscriptEntry::ToolResult { tool, ok, .. } => {
            let mark = if *ok { "✓" } else { "✗" };
            vec![format!("\x1b[33m{mark} {tool}\x1b[0m")]
        }
        TranscriptEntry::Plan { items } => items
            .iter()
            .map(|i| format!("\x1b[35m• {i}\x1b[0m"))
            .collect(),
        TranscriptEntry::Status { msg } => vec![format!("\x1b[2m[{msg}]\x1b[0m")],
    }
}

fn draw(state: &State) {
    let (rows, cols) = term_size();
    let body_rows = rows.saturating_sub(3);
    let mut out = io::stdout().lock();
    write!(out, "\x1b[2J\x1b[H").ok();

    // header
    writeln!(
        out,
        "\x1b[7m sy agt inspector  ·  status: {} {}\x1b[0m\r",
        state.status,
        " ".repeat(cols.saturating_sub(40))
    )
    .ok();

    // transcript: scroll_offset == 0 means we render the latest body_rows
    // entries; positive offset shifts the window backwards.
    let max_offset = state.transcript.len().saturating_sub(body_rows);
    let offset = state.scroll_offset.min(max_offset);
    let end = state.transcript.len().saturating_sub(offset);
    let start = end.saturating_sub(body_rows);
    for line in &state.transcript[start..end] {
        let truncated: String = line.chars().take(cols).collect();
        writeln!(out, "{truncated}\r").ok();
    }
    // pad the rest
    for _ in (end - start)..body_rows {
        writeln!(out, "\r").ok();
    }

    // permission / hint / scroll indicator
    if let Some(p) = &state.permission {
        writeln!(out, "\x1b[41;97m permission: {p} \x1b[0m\r").ok();
    } else if offset > 0 {
        writeln!(
            out,
            "\x1b[7m ↑ scrolled {offset} lines · End/Enter snaps live · PgUp/PgDn move \x1b[0m\r"
        )
        .ok();
    } else {
        writeln!(
            out,
            "\x1b[2m─ Enter sends · PgUp/PgDn scroll · Ctrl+C stops · Ctrl+D detaches\x1b[0m\r"
        )
        .ok();
    }

    // input box
    write!(out, "\x1b[K> {}\r", state.input).ok();
    write!(out, "\x1b[{};{}H", rows, state.input.len() + 3).ok();
    out.flush().ok();
}

fn term_size() -> (usize, usize) {
    use std::os::raw::c_int;
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        _pad: [u16; 2],
    }
    extern "C" {
        fn ioctl(fd: c_int, req: u64, ...) -> c_int;
    }
    let mut ws = Winsize {
        ws_row: 24,
        ws_col: 80,
        _pad: [0; 2],
    };
    const TIOCGWINSZ: u64 = 0x5413;
    unsafe {
        ioctl(1, TIOCGWINSZ, &mut ws as *mut _);
    }
    (ws.ws_row as usize, ws.ws_col as usize)
}

fn stty_save() -> Option<String> {
    let out = Command::new("stty").arg("-g").output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn stty_raw() {
    let _ = Command::new("stty").args(["raw", "-echo"]).status();
}

fn stty_restore(saved: &str) {
    let _ = Command::new("stty").arg(saved).status();
}
