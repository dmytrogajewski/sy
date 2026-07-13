//! Tolerant fallback parser for a Telegram `messages*.html` export.
//!
//! Used when the JSON path yields nothing (the file is HTML, or the JSON is
//! unparseable). Telegram's HTML wraps each message in
//! `<div class="message ...">` with a `div.from_name`, a `div.date` whose
//! `title` attribute holds the timestamp, an optional `div.reply_to`, and a
//! `div.text` body; media appears as `<a>`/`<img>` links. The parser is
//! deliberately string-based (no HTML crate) and never panics on malformed
//! markup — a block it cannot read contributes an empty/partial message
//! rather than aborting the pass.

use super::Message;

const MESSAGE_MARKER: &str = "<div class=\"message";

/// Parse the message blocks of a Telegram HTML export into [`Message`]s.
pub(super) fn parse(text: &str) -> Vec<Message> {
    split_blocks(text).iter().map(|b| message(b)).collect()
}

/// Split on the `<div class="message...` marker; each block runs to the next
/// marker (or end of input).
fn split_blocks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(MESSAGE_MARKER) {
        let after = &rest[i + MESSAGE_MARKER.len()..];
        let end = after.find(MESSAGE_MARKER).unwrap_or(after.len());
        out.push(&after[..end]);
        rest = &after[end..];
    }
    out
}

fn message(block: &str) -> Message {
    Message {
        id: None,
        date: title_of(block, "date"),
        from: inner_text(block, "from_name"),
        reply_to_id: None,
        has_media: block.contains("<a class=\"media") || block.contains("<img"),
        voice_media: voice_href(block),
        text: inner_text(block, "text").unwrap_or_default(),
    }
}

/// Export-relative href of a voice note / round video, if the block links
/// one. Telegram HTML exports point these at `voice_messages/` and
/// `round_video_messages/`.
fn voice_href(block: &str) -> Option<String> {
    for dir in ["voice_messages/", "round_video_messages/"] {
        if let Some(start) = block.find(dir) {
            let tail = &block[start..];
            let end = tail.find(['"', '\'']).unwrap_or(tail.len());
            return Some(tail[..end].to_string());
        }
    }
    None
}

/// Inner text of the first `<div class="<class> ...">…</div>` in `block`,
/// with tags stripped and whitespace trimmed.
fn inner_text(block: &str, class: &str) -> Option<String> {
    let body = div_body(block, class)?;
    let stripped = strip_tags(&body);
    let trimmed = stripped.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The `title="..."` attribute of the first `<div class="<class> ...">`.
fn title_of(block: &str, class: &str) -> Option<String> {
    let needle = format!("class=\"{class}");
    let at = block.find(&needle)?;
    let tag_end = block[at..].find('>')?;
    let open_tag = &block[at..at + tag_end];
    let ti = open_tag.find("title=\"")?;
    let after = &open_tag[ti + "title=\"".len()..];
    let close = after.find('"')?;
    Some(after[..close].to_string())
}

/// The inner HTML between the opening `<div class="<class> ...">` and its
/// matching (depth-aware) `</div>`.
fn div_body(block: &str, class: &str) -> Option<String> {
    let needle = format!("class=\"{class}");
    let at = block.find(&needle)?;
    let after_open = at + block[at..].find('>')? + 1;
    let mut depth = 1i32;
    let mut rest = &block[after_open..];
    let mut consumed = after_open;
    while depth > 0 {
        let next_open = rest.find("<div");
        let next_close = rest.find("</div>")?;
        match next_open {
            Some(o) if o < next_close => {
                depth += 1;
                consumed += o + 4;
                rest = &block[consumed..];
            }
            _ => {
                depth -= 1;
                if depth == 0 {
                    return Some(block[after_open..consumed + next_close].to_string());
                }
                consumed += next_close + "</div>".len();
                rest = &block[consumed..];
            }
        }
    }
    None
}

/// Remove HTML tags, leaving text content.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}
