//! `fs::mime` — MIME sniffer (extension first, then `tree_magic_mini`
//! on the first 8 KiB). Step 19 of the [`sy-file-manager`
//! roadmap][roadmap] / SPEC §3.3 item 11 lands the ladder
//! [`mime_for`] (used by the previewer routing in Step 23+ and by
//! the IPC layer's [`Entry::mime_hint`] population in Step 20+).
//!
//! ## Detection ladder
//!
//! 1. **Extension lookup** via [`xdg_mime::SharedMimeInfo::get_mime_types_from_file_name`].
//!    The shared MIME-info DB ships in `/usr/share/mime/` on Fedora
//!    43 (and the GNOME / KDE stacks). The crate returns
//!    `application/octet-stream` when the extension is unknown — we
//!    treat that as "ladder miss" and fall through to step 2.
//! 2. **Magic-number sniff** of the first 8 KiB via
//!    [`tree_magic_mini::from_u8`]. `tree_magic_mini` ships the
//!    freedesktop magic table inline so this step does not depend on
//!    a system MIME DB being installed.
//! 3. **Fallback** to `application/octet-stream` if both steps fail.
//!    [`mime_for`] never panics and never returns `Err` on a
//!    well-formed input path; the `Result` shape is reserved for I/O
//!    errors on the 8 KiB read.
//!
//! ## Caching
//!
//! The [`xdg_mime::SharedMimeInfo`] instance is heavyweight (parses
//! every `/usr/share/mime/` glob + magic file at construction) so we
//! cache it in a process-global [`OnceLock`]. Same precedent as
//! cosmic-text's shaper cache in `sy-plugin-md`.
//!
//! ## Limitations
//!
//! On a stripped container without `/usr/share/mime/`, the
//! extension-lookup step degrades to "always returns
//! `application/octet-stream`" — that's the documented best-effort
//! fallback. The sniff step still works because `tree_magic_mini`
//! carries its own magic table.
//!
//! [roadmap]: ../../../../specs/roadmaps/sy-file-manager/ROADMAP.md
//! [`Entry::mime_hint`]: crate::file::state::panes::Entry::mime_hint

use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use xdg_mime::SharedMimeInfo;

/// Size of the leading-bytes window we feed to `tree_magic_mini` on
/// the sniff fallback. The freedesktop magic-number table caps its
/// rule offsets in the low single-digit KiB; 8 KiB covers every rule
/// in the table with budget to spare. Named so call sites and tests
/// share one source of truth.
const SNIFF_WINDOW_BYTES: usize = 8 * 1024;

/// The MIME string we return when both ladder rungs miss. Matches
/// the freedesktop convention so downstream routing layers can
/// match-on a single literal.
const APPLICATION_OCTET_STREAM: &str = "application/octet-stream";

/// Cache of the parsed `/usr/share/mime/` DB. `SharedMimeInfo::new()`
/// reads the entire shared MIME-info tree once, which is expensive
/// (~5-15 ms on a warm cache); the cache makes the steady-state
/// `mime_for` call cost dominated by the per-path glob lookup
/// (microseconds).
static MIME_DB: OnceLock<SharedMimeInfo> = OnceLock::new();

fn mime_db() -> &'static SharedMimeInfo {
    MIME_DB.get_or_init(SharedMimeInfo::new)
}

/// Resolve a MIME type for `path` using the extension-first / sniff-
/// fallback ladder described in the module doc. Returns the
/// freedesktop MIME string (e.g. `"text/markdown"`, `"image/png"`).
///
/// Never panics. Returns `Err` only if the sniff fallback's 8 KiB
/// read fails with an I/O error other than EOF; on EOF (empty file)
/// the function returns `application/octet-stream`.
pub fn mime_for(path: &Path) -> Result<String> {
    if let Some(m) = lookup_by_extension(path) {
        return Ok(m);
    }
    sniff_first_window(path)
}

/// Step 1 of the ladder. Returns `Some(mime)` when the extension is
/// unambiguous; `None` when the DB has no glob for the name OR when
/// the lookup degraded to `application/octet-stream` (which the
/// crate uses as its "I don't know" sentinel). The caller falls
/// through to the sniff step on `None`.
fn lookup_by_extension(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let hits = mime_db().get_mime_types_from_file_name(name);
    let top = hits.into_iter().next()?;
    let s = top.essence_str().to_string();
    if s == APPLICATION_OCTET_STREAM {
        None
    } else {
        Some(s)
    }
}

/// Step 2 of the ladder. Reads up to [`SNIFF_WINDOW_BYTES`] from
/// `path` (note: **not** mmap — `Read::read` keeps the syscall
/// footprint minimal and avoids the EBADF / SIGBUS class of bugs
/// mmap brings on remote filesystems). On an empty file or any
/// truncated read, `tree_magic_mini::from_u8` either returns a
/// concrete match or falls through to `application/octet-stream`.
fn sniff_first_window(path: &Path) -> Result<String> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(APPLICATION_OCTET_STREAM.into()),
    };
    let mut buf = vec![0_u8; SNIFF_WINDOW_BYTES];
    let n = f
        .read(&mut buf)
        .with_context(|| format!("fs::mime sniff read({path:?}) failed"))?;
    buf.truncate(n);
    Ok(tree_magic_mini::from_u8(&buf).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Ladder step 1 — extension lookup is the fast-path. A
    /// freshly-created `foo.md` returns `text/markdown` without
    /// the sniffer reading any bytes.
    #[test]
    fn extension_first_then_sniff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("foo.md");
        std::fs::write(&p, b"x").expect("write");
        let m = mime_for(&p).expect("mime_for must succeed");
        assert_eq!(
            m, "text/markdown",
            "extension `.md` must resolve to text/markdown via xdg-mime"
        );
    }

    /// Ladder step 2 — extension-less plaintext falls through to the
    /// sniff window and gets recognised as `text/plain` by
    /// `tree_magic_mini`'s magic table.
    #[test]
    fn extensionless_text_sniffed_as_text_plain() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("noext");
        let body = "hello world\n".repeat(100);
        std::fs::write(&p, body.as_bytes()).expect("write");
        let m = mime_for(&p).expect("mime_for must succeed");
        assert_eq!(
            m, "text/plain",
            "extension-less ascii body must sniff as text/plain"
        );
    }

    /// Ladder step 2 — a minimal PNG header byte sequence followed
    /// by garbage sniffs as `image/png`. The file is named `noext`
    /// to force the sniff path (so we're actually exercising
    /// `tree_magic_mini::from_u8`, not the extension shortcut).
    #[test]
    fn png_sniffed_correctly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("noext");
        let mut f = std::fs::File::create(&p).expect("create png");
        // PNG magic header: 8 bytes.
        f.write_all(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'])
            .expect("write png header");
        f.write_all(&[0_u8; 64]).expect("write png trailer garbage");
        let m = mime_for(&p).expect("mime_for must succeed");
        assert_eq!(
            m, "image/png",
            "PNG header bytes must sniff as image/png via tree_magic_mini"
        );
    }
}
