//! Three-pane state model. Step 14 of the
//! [`sy-file-manager` roadmap][roadmap] models the journey-J2
//! "3-pane render populated from real fs" surface as pure data — no
//! I/O lives here; Step 15's `fs::walk` will be what populates
//! `Pane::entries`.
//!
//! [roadmap]: ../../../../specs/roadmaps/sy-file-manager/ROADMAP.md

use std::path::PathBuf;
use std::time::SystemTime;

use super::selection::EntryId;

/// Pane discriminator. The journey-J2 layout has three slots; the
/// responsive ladder (Step 23) can hide the parent / preview ones but
/// the id space stays fixed so IPC consumers (Step 20+) reference panes
/// by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaneId {
    /// The directory above [`PaneId::Current`]. Hidden in 1-/2-pane.
    Parent,
    /// The cwd the user is browsing. Always visible.
    Current,
    /// Either the cursor target's children (when it's a dir) or a
    /// previewer render (when it's a file). Step 19+ binds this up.
    Preview,
}

/// Coarse-grained kind tag for [`Entry`]. The full mime sniff lives on
/// `Entry::size`-adjacent fields once Step 19's `fs::mime` lands; today
/// we only carry the kind the pane renderer needs to pick an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link. The target's actual kind is irrelevant here —
    /// `Entry::is_symlink` is the discriminator and `Entry::broken_link`
    /// distinguishes broken vs valid.
    Symlink,
    /// Anything else (block dev, char dev, fifo, socket). The pane
    /// renderer maps this to a generic icon; the file ops UI refuses to
    /// copy/move these (SPEC §3.3 row 5).
    Other,
}

/// A single row in a pane. Matches the SPEC §3.1 `state/` shape that
/// Step 15's `fs::walk` will produce; carrying [`SystemTime`] (not a
/// formatted string) lets the sort/view layer pick its own formatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable id within the parent pane. Step 15 generates these as a
    /// monotonic counter; tests can pick any unique u64.
    pub id: EntryId,
    /// Display name (basename, no path component). Bytes-faithful: the
    /// SPEC §4.4 `utf8_unfriendly_names_listed_as_bytes` invariant
    /// keeps this a `String` only because lossy decode is the agreed
    /// fallback the pane renderer handles.
    pub name: String,
    /// File/dir/symlink/other discriminator.
    pub kind: EntryKind,
    /// Size in bytes; 0 for directories (`fs::walk` does not recurse).
    pub size: u64,
    /// Last modification time, from `statx`.
    pub mtime: SystemTime,
    /// True when the underlying inode is a symlink. Independent of
    /// `kind` (which reports the kind of the link target).
    pub is_symlink: bool,
    /// True when [`Entry::is_symlink`] AND the target does not resolve.
    /// The pane renderer paints this in a "warning" tint.
    pub broken_link: bool,
    /// True when the user has at least `R` on the entry (and `RX` on a
    /// dir). Read-deny rows show a lock glyph and reject enter.
    pub readable: bool,
    /// Coarse mime hint (e.g. `Some("text/markdown")`) when Step 19's
    /// `fs::mime` can derive one from the extension or the leading
    /// bytes — `None` from Step 15's `fs::walk`, which only sniffs by
    /// extension. The preview pane reads this to pick a renderer
    /// without re-stat-ing the file.
    pub mime_hint: Option<String>,
    /// Symlink target the inode points at, as the raw `readlink`
    /// payload. `Some(_)` iff [`Entry::is_symlink`] is `true`; the
    /// display layer renders it as `name -> target`. Step 15's walk
    /// fills this from `readlink(2)`; broken-link detection compares
    /// the target's `lstat` to `stat` (the latter ENOENT-ing).
    pub symlink_target: Option<PathBuf>,
}

/// One column of the three-pane layout. The cursor + scroll pair are
/// the two ints that survive a `walk()` refresh (we want the cursor to
/// stay roughly where it was when `entries` reshuffles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// The directory this pane lists.
    pub cwd: PathBuf,
    /// Rendered rows. Step 15's `fs::walk` will sort these (mtime
    /// desc by default per SPEC §3.3); Step 14 takes whatever the
    /// caller hands in.
    pub entries: Vec<Entry>,
    /// Cursor row inside [`Pane::entries`]. [`Pane::set_entries`]
    /// clamps this to `len - 1` on refresh.
    pub cursor: usize,
    /// First visible row in the scrollable viewport.
    pub scroll: usize,
}

impl Pane {
    /// Empty pane rooted at `cwd`. Step 15's `fs::walk` populates
    /// `entries` post-construct via [`Pane::set_entries`].
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
        }
    }

    /// Refresh the pane contents and clamp the cursor so it never
    /// dangles past the new last row. Called from every `walk()`
    /// completion + every filter-change. Clamping is encapsulated here
    /// so call sites (Step 15+) don't each re-derive the saturating
    /// arithmetic.
    pub fn set_entries(&mut self, new: Vec<Entry>) {
        self.entries = new;
        if self.entries.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len() - 1;
        }
        // Scroll never lags below cursor, but the pane view layer
        // (Step 23+) is what decides the visible window; clamping
        // scroll to `<= cursor` here would over-constrain that.
    }
}

/// The three-pane bundle that lives on [`super::State`]. Each slot is
/// always present; the responsive layout (Step 23+) decides which to
/// draw. Keeping them as named fields (not an array) lets call sites
/// say `state.panes.current` rather than `state.panes[PaneId::Current]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panes {
    pub parent: Pane,
    pub current: Pane,
    pub preview: Pane,
}

impl Panes {
    /// Build the bundle from explicit cwds. Step 15+ calls this at app
    /// startup with `(parent_of_cwd, cwd, child_of_cursor)` from the
    /// CLI arg.
    pub fn new(parent: PathBuf, current: PathBuf, preview: PathBuf) -> Self {
        Self {
            parent: Pane::new(parent),
            current: Pane::new(current),
            preview: Pane::new(preview),
        }
    }
}

impl Default for Panes {
    /// Empty `/` panes. Lets [`super::State::default`] keep working for
    /// the `state_marker_is_constructable` smoke test even though Step
    /// 14 wires real fields into `State`.
    fn default() -> Self {
        Self::new(PathBuf::from("/"), PathBuf::from("/"), PathBuf::from("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cursor must clamp to `len - 1` after a refresh that shrinks the
    /// row count below the previous cursor index. Journey-J2 + J7 both
    /// depend on this — without it, the GUI would render an
    /// out-of-bounds cursor row after every `walk()` + filter cycle.
    #[test]
    fn cursor_clamps_on_entries_change() {
        let mut pane = Pane::new(PathBuf::from("/tmp"));
        pane.cursor = 9;
        let three = vec![
            sample_entry(0, "a.txt"),
            sample_entry(1, "b.txt"),
            sample_entry(2, "c.txt"),
        ];
        pane.set_entries(three);
        assert_eq!(
            pane.cursor, 2,
            "cursor must clamp to len-1 = 2 when entries shrink to 3 rows"
        );

        pane.set_entries(Vec::new());
        assert_eq!(pane.cursor, 0, "cursor must collapse to 0 on empty refresh");
    }

    fn sample_entry(id: EntryId, name: &str) -> Entry {
        Entry {
            id,
            name: name.to_owned(),
            kind: EntryKind::File,
            size: 0,
            mtime: SystemTime::UNIX_EPOCH,
            is_symlink: false,
            broken_link: false,
            readable: true,
            mime_hint: None,
            symlink_target: None,
        }
    }
}
