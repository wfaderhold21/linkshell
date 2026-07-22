//! Recursive pane layout tree.
//!
//! The output zone is a binary tree of splits. Each `Leaf` occupies one pane
//! slot; the app keeps parallel vectors (`panes`, `pane_sizes`) indexed by the
//! tree's in-order leaf position, so leaf N in `rects()` corresponds to
//! `panes[N]`. This lets a pane be split any number of times, in either
//! direction, instead of the old fixed two-pane layout.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// How a split divides its area between its two children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Children side by side, left | right (a vertical divider).
    Row,
    /// Children stacked, top / bottom (a horizontal divider).
    Col,
}

impl SplitDir {
    pub fn flip(self) -> SplitDir {
        match self {
            SplitDir::Row => SplitDir::Col,
            SplitDir::Col => SplitDir::Row,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LayoutTree {
    #[default]
    Leaf,
    Split {
        dir: SplitDir,
        first: Box<LayoutTree>,
        second: Box<LayoutTree>,
    },
}

impl LayoutTree {
    /// Number of pane slots (leaves) in the tree.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn leaf_count(&self) -> usize {
        match self {
            LayoutTree::Leaf => 1,
            LayoutTree::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    /// Rectangles for each leaf, in in-order traversal order (matching the
    /// app's `panes` vector).
    pub fn rects(&self, area: Rect) -> Vec<Rect> {
        let mut out = Vec::new();
        self.collect_rects(area, &mut out);
        out
    }

    fn collect_rects(&self, area: Rect, out: &mut Vec<Rect>) {
        match self {
            LayoutTree::Leaf => out.push(area),
            LayoutTree::Split { dir, first, second } => {
                let direction = match dir {
                    SplitDir::Row => Direction::Horizontal,
                    SplitDir::Col => Direction::Vertical,
                };
                let chunks = Layout::default()
                    .direction(direction)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(area);
                first.collect_rects(chunks[0], out);
                second.collect_rects(chunks[1], out);
            }
        }
    }

    /// Split the leaf at in-order position `index` into two, keeping the
    /// existing content in the first child. A new empty leaf appears at
    /// `index + 1`. No-op if `index` is out of range.
    pub fn split_leaf(&mut self, index: usize, dir: SplitDir) {
        let mut counter = 0;
        self.split_at(index, dir, &mut counter);
    }

    fn split_at(&mut self, target: usize, dir: SplitDir, counter: &mut usize) -> bool {
        match self {
            LayoutTree::Leaf => {
                if *counter == target {
                    *self = LayoutTree::Split {
                        dir,
                        first: Box::new(LayoutTree::Leaf),
                        second: Box::new(LayoutTree::Leaf),
                    };
                    return true;
                }
                *counter += 1;
                false
            }
            LayoutTree::Split { first, second, .. } => {
                first.split_at(target, dir, counter) || second.split_at(target, dir, counter)
            }
        }
    }

    /// Remove the leaf at in-order position `index`; its sibling subtree
    /// expands to reclaim the space. Returns false (no change) if `index` is
    /// the sole remaining leaf or out of range.
    pub fn close_leaf(&mut self, index: usize) -> bool {
        if matches!(self, LayoutTree::Leaf) {
            return false;
        }
        let mut counter = 0;
        self.close_at(index, &mut counter)
    }

    fn close_at(&mut self, target: usize, counter: &mut usize) -> bool {
        let LayoutTree::Split { first, second, .. } = self else {
            *counter += 1;
            return false;
        };
        // First child.
        if matches!(**first, LayoutTree::Leaf) {
            if *counter == target {
                let sibling = std::mem::take(second.as_mut());
                *self = sibling;
                return true;
            }
            *counter += 1;
        } else if first.close_at(target, counter) {
            return true;
        }
        // Second child (re-borrow after the possible mutation above).
        let LayoutTree::Split { first, second, .. } = self else {
            return false;
        };
        if matches!(**second, LayoutTree::Leaf) {
            if *counter == target {
                let sibling = std::mem::take(first.as_mut());
                *self = sibling;
                return true;
            }
            *counter += 1;
        } else if second.close_at(target, counter) {
            return true;
        }
        false
    }

    /// Flip the direction of the split that directly parents the leaf at
    /// in-order position `index`.
    pub fn rotate_leaf(&mut self, index: usize) -> bool {
        let mut counter = 0;
        self.rotate_at(index, &mut counter)
    }

    fn rotate_at(&mut self, target: usize, counter: &mut usize) -> bool {
        let LayoutTree::Split { dir, first, second } = self else {
            *counter += 1;
            return false;
        };
        if matches!(**first, LayoutTree::Leaf) {
            if *counter == target {
                *dir = dir.flip();
                return true;
            }
            *counter += 1;
        } else if first.rotate_at(target, counter) {
            return true;
        }
        if matches!(**second, LayoutTree::Leaf) {
            if *counter == target {
                *dir = dir.flip();
                return true;
            }
            *counter += 1;
        } else if second.rotate_at(target, counter) {
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_leaf_covers_the_whole_area() {
        let tree = LayoutTree::Leaf;
        let area = Rect::new(0, 0, 100, 40);
        assert_eq!(tree.rects(area), vec![area]);
        assert_eq!(tree.leaf_count(), 1);
    }

    #[test]
    fn split_grows_leaf_count_and_targets_the_right_leaf() {
        let mut tree = LayoutTree::Leaf;
        tree.split_leaf(0, SplitDir::Row); // 2 leaves
        assert_eq!(tree.leaf_count(), 2);
        tree.split_leaf(0, SplitDir::Col); // split the first → 3 leaves
        assert_eq!(tree.leaf_count(), 3);
    }

    #[test]
    fn three_pane_rects_tile_without_overlap_or_gap() {
        // Leaf0 split Row → [0,1]; split leaf0 Col → top/bottom + right.
        let mut tree = LayoutTree::Leaf;
        tree.split_leaf(0, SplitDir::Row);
        tree.split_leaf(0, SplitDir::Col);
        let area = Rect::new(0, 0, 100, 40);
        let rects = tree.rects(area);
        assert_eq!(rects.len(), 3);
        let covered: u16 = rects.iter().map(|r| r.area()).sum();
        assert_eq!(covered, area.area(), "panes tile the area exactly");
        // No two rects overlap.
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(a.intersection(*b).area() == 0, "panes must not overlap");
            }
        }
    }

    #[test]
    fn close_collapses_to_sibling() {
        let mut tree = LayoutTree::Leaf;
        tree.split_leaf(0, SplitDir::Row);
        tree.split_leaf(0, SplitDir::Col);
        assert_eq!(tree.leaf_count(), 3);
        assert!(tree.close_leaf(1));
        assert_eq!(tree.leaf_count(), 2);
        // Closing down to a single leaf is allowed…
        assert!(tree.close_leaf(0));
        assert_eq!(tree.leaf_count(), 1);
        // …but the final leaf cannot be closed.
        assert!(!tree.close_leaf(0));
    }

    #[test]
    fn rotate_flips_the_parent_split_only() {
        let mut tree = LayoutTree::Leaf;
        tree.split_leaf(0, SplitDir::Row);
        assert!(tree.rotate_leaf(0));
        match &tree {
            LayoutTree::Split { dir, .. } => assert_eq!(*dir, SplitDir::Col),
            _ => panic!("expected a split"),
        }
    }
}
