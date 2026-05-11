use std::{
    io::{self, Read, Write},
    process::Command,
};

use anyhow::{Context, Result};

/// Interactive terminal calendar. Renders the current month using `cal(1)`,
/// highlights today, and accepts single-key navigation:
///
///   h / p / ,   previous month
///   l / n / .   next month
///   k           previous year
///   j           next year
///   t           jump back to today
///   q / ESC     quit
pub fn run() -> Result<()> {
    let (ny, nm, nd) = today();
    let mut y = ny;
    let mut m = nm;

    let saved = stty_save().ok();
    stty_raw().ok();

    let result = event_loop(&mut y, &mut m, ny, nm, nd);

    if let Some(s) = saved.as_deref() {
        stty_restore(s);
    } else {
        let _ = Command::new("stty").arg("sane").status();
    }
    let _ = write!(io::stdout(), "\x1b[?25h\x1b[0m\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
    result
}

fn event_loop(y: &mut i32, m: &mut u32, ny: i32, nm: u32, nd: u32) -> Result<()> {
    loop {
        render(*y, *m, ny, nm, nd)?;
        let mut buf = [0u8; 1];
        let n = io::stdin().read(&mut buf).context("read stdin")?;
        if n == 0 {
            break;
        }
        match buf[0] {
            b'q' | b'Q' | 0x1b | 0x03 | 0x04 => break,
            b'h' | b'p' | b'P' | b',' | b'<' => {
                let (ny2, nm2) = prev_month(*y, *m);
                *y = ny2;
                *m = nm2;
            }
            b'l' | b'n' | b'N' | b'.' | b'>' => {
                let (ny2, nm2) = next_month(*y, *m);
                *y = ny2;
                *m = nm2;
            }
            b'k' | b'K' => *y -= 1,
            b'j' | b'J' => *y += 1,
            b't' | b'T' => {
                *y = ny;
                *m = nm;
            }
            _ => {}
        }
    }
    Ok(())
}

fn render(y: i32, m: u32, ny: i32, nm: u32, nd: u32) -> Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b[?25l\x1b[2J\x1b[H")?;

    writeln!(out, "  \x1b[1m{}\x1b[0m\r", header(y, m))?;
    writeln!(out, "\r")?;

    let cal = Command::new("cal")
        .args(["-m", &m.to_string(), &y.to_string()])
        .output()
        .context("spawn cal(1)")?;
    if !cal.status.success() {
        writeln!(out, "  cal(1) failed\r")?;
    }

    let s = String::from_utf8_lossy(&cal.stdout);
    let is_now = y == ny && m == nm;
    for (i, line) in s.lines().enumerate() {
        if i == 0 {
            // Skip cal(1)'s own "    April 2026" heading; ours replaces it.
            continue;
        }
        let rendered = if is_now {
            highlight_today(line, nd)
        } else {
            line.to_string()
        };
        writeln!(out, "  {}\r", rendered)?;
    }

    writeln!(out, "\r")?;
    writeln!(
        out,
        "  \x1b[2mh/p prev  l/n next  j/k year  t today  q quit\x1b[0m\r"
    )?;
    out.flush()?;
    Ok(())
}

fn highlight_today(line: &str, day: u32) -> String {
    let needle = format!("{:>2}", day);
    let bytes = line.as_bytes();
    let nd_len = needle.len();
    let n = line.len();
    let mut out = String::with_capacity(n + 8);
    let mut i = 0;
    while i < n {
        if i + nd_len <= n && &bytes[i..i + nd_len] == needle.as_bytes() {
            let before_ok = i == 0 || bytes[i - 1] == b' ';
            let after_ok = i + nd_len == n || bytes[i + nd_len] == b' ';
            if before_ok && after_ok {
                out.push_str("\x1b[7m");
                out.push_str(&needle);
                out.push_str("\x1b[0m");
                i += nd_len;
                continue;
            }
        }
        // Advance by one UTF-8 scalar so cyrillic weekday headers aren't shredded.
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn header(y: i32, m: u32) -> String {
    // Ask `date` for a locale-formatted "Month YYYY" so the header matches
    // cal(1)'s weekday row (both follow LC_TIME).
    let spec = format!("{y:04}-{m:02}-01");
    let out = Command::new("date")
        .args(["-d", &spec, "+%B %Y"])
        .output()
        .ok();
    if let Some(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    format!("{y}-{m:02}")
}

fn prev_month(y: i32, m: u32) -> (i32, u32) {
    if m == 1 {
        (y - 1, 12)
    } else {
        (y, m - 1)
    }
}

fn next_month(y: i32, m: u32) -> (i32, u32) {
    if m == 12 {
        (y + 1, 1)
    } else {
        (y, m + 1)
    }
}

fn today() -> (i32, u32, u32) {
    let out = Command::new("date").arg("+%Y %m %d").output().ok();
    if let Some(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout);
            let mut it = s.split_whitespace();
            let y = it.next().and_then(|x| x.parse().ok()).unwrap_or(1970);
            let m = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
            let d = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
            return (y, m, d);
        }
    }
    (1970, 1, 1)
}

fn stty_save() -> Result<String> {
    let out = Command::new("stty").arg("-g").output().context("stty -g")?;
    if !out.status.success() {
        anyhow::bail!("stty -g failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn stty_raw() -> Result<()> {
    Command::new("stty")
        .args(["-icanon", "-echo", "min", "1", "time", "0"])
        .status()
        .context("stty raw")?;
    Ok(())
}

fn stty_restore(saved: &str) {
    let _ = Command::new("stty").arg(saved).status();
}
