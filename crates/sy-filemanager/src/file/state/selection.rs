//! Multi-select set keyed by [`EntryId`]. Step 14 of the
//! [`sy-file-manager` roadmap][roadmap] lands the journey-J5 surface:
//! `<Space>` toggle, `<Shift>+arrow` range, `*` all, `a` invert, `<Esc>`
//! clear. Backed by a [`BTreeSet`] so `invert(&universe)` is observably
//! ordered by id — the journey-J5 acceptance criterion
//! `invert_preserves_order_by_id` only passes against a sorted set.
//!
//! [roadmap]: ../../../../specs/roadmaps/sy-file-manager/ROADMAP.md

use std::collections::BTreeSet;

/// Stable identifier for a [`super::panes::Entry`]. SPEC §3.1 keeps the
/// id space numeric so the IPC + MCP surfaces (Step 20+) can ship it as
/// a JSON number; `u64` is wide enough that a single pane can never run
/// out (one entry per nanosecond for >580 years).
pub type EntryId = u64;

/// Multi-select set as stored on [`super::State`]. Ordered iteration is
/// part of the contract (the journey-J5 invert beat reads it back via
/// [`BTreeSet::iter`]); a `HashSet` would silently break the wire shape
/// Step 20's IPC `selection.list` op will deliver to agents.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SelectionSet {
    /// Inner ordered id set. Public-in-module so the parent `state`
    /// crate can introspect for snapshot serialisation later (Step 20);
    /// outside callers must go through the methods so the invariants
    /// (ordering, idempotency) stay enforced.
    pub(crate) ids: BTreeSet<EntryId>,
}

impl SelectionSet {
    /// Construct an empty selection. Equivalent to
    /// [`SelectionSet::default`] but reads more naturally at call sites
    /// (`SelectionSet::new()` next to `Pane::new(...)`).
    pub fn new() -> Self {
        Self {
            ids: BTreeSet::new(),
        }
    }

    /// Toggle membership of `id`. Idempotent — calling twice returns
    /// the set to its prior state, which is the journey-J5 `<Space>`
    /// double-tap invariant.
    pub fn toggle(&mut self, id: EntryId) {
        if !self.ids.remove(&id) {
            self.ids.insert(id);
        }
    }

    /// Inclusive range insert: `[a, b]` (or `[b, a]` if reversed). Maps
    /// onto the journey-J5 `<Shift>+arrow` beat; the GUI passes the
    /// anchor cursor and the current cursor as `(a, b)` in either order
    /// depending on direction, so the impl must handle both.
    pub fn add_range(&mut self, a: EntryId, b: EntryId) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        for id in lo..=hi {
            self.ids.insert(id);
        }
    }

    /// Replace `self` with `universe \ self`. Iteration order is
    /// preserved ascending because the backing [`BTreeSet`] sorts on
    /// insert.
    pub fn invert(&mut self, universe: &[EntryId]) {
        let mut next = BTreeSet::new();
        for id in universe {
            if !self.ids.contains(id) {
                next.insert(*id);
            }
        }
        self.ids = next;
    }

    /// Select every id in the universe (journey-J5 `*` beat).
    pub fn all(&mut self, universe: &[EntryId]) {
        self.ids = universe.iter().copied().collect();
    }

    /// Drop every selection (journey-J5 `<Esc>` beat).
    pub fn clear(&mut self) {
        self.ids.clear();
    }

    /// Membership query. Pane rendering (Step 23+) calls this once per
    /// row to drive the selection chevron in the gutter.
    pub fn contains(&self, id: EntryId) -> bool {
        self.ids.contains(&id)
    }

    /// Selection cardinality. Statusbar chip (Step 25) reads this.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// True when nothing is selected. Clippy nudges `len() == 0` users
    /// here, and Step 20's IPC `selection.list` skips its serialisation
    /// pass when the set is empty.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Ordered iterator over the selected ids. Step 20 will ship this
    /// over JSON as a sorted array; locking the order at the type level
    /// means consumers (agents, MCP) can binary-search the result.
    pub fn iter(&self) -> std::collections::btree_set::Iter<'_, EntryId> {
        self.ids.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 14 DoD: `<Space>` double-tap is a no-op. The journey-J5
    /// beat depends on this — a user toggles, mis-clicks, toggles back,
    /// and expects the original selection.
    #[test]
    fn toggle_idempotent() {
        let mut s = SelectionSet::new();
        s.toggle(7);
        let after_first = s.clone();
        s.toggle(7);
        s.toggle(7);
        assert_eq!(
            s, after_first,
            "two extra toggles must collapse to a single one"
        );
    }

    /// `add_range(a, b)` covers both endpoints + every integer between.
    /// Journey-J5 `<Shift>+arrow` beat.
    #[test]
    fn add_range_inclusive() {
        let mut s = SelectionSet::new();
        s.add_range(3, 6);
        for id in 3..=6 {
            assert!(s.contains(id), "id {id} must be in [3,6] inclusive range");
        }
        assert_eq!(s.len(), 4, "inclusive range [3,6] has 4 elements");
        // Reversed range — same shape.
        let mut t = SelectionSet::new();
        t.add_range(6, 3);
        assert_eq!(t, s, "reversed range must match forward range");
    }

    /// Invert on a 5-element universe with a 2-element selection
    /// returns exactly the 3 complement ids, ascending. Iteration via
    /// `BTreeSet::iter` is part of the contract.
    #[test]
    fn invert_preserves_order_by_id() {
        let universe: [EntryId; 5] = [1, 2, 3, 4, 5];
        let mut s = SelectionSet::new();
        s.toggle(2);
        s.toggle(4);
        s.invert(&universe);
        let observed: Vec<EntryId> = s.iter().copied().collect();
        assert_eq!(
            observed,
            vec![1, 3, 5],
            "invert(complement) must yield the remaining ids in ascending order"
        );
    }
}
