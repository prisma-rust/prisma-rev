//! What the search picked last time, so it can favour continuing over hopping.
//!
//! Pure nearest-neighbour lookup happily switches between frames that are a
//! fraction of a millimetre apart, which reads as a stutter: two neighbouring
//! samples of the same walk can have nearly identical cost, and the winner flips
//! every few frames. Real motion matching keeps the decision sticky in two ways
//! and both are expressed here as cost multipliers rather than special cases in
//! the loop:
//!
//! - the frame that is currently playing gets a bonus (cheap to keep running);
//! - frames picked very recently get a penalty (no ping-pong between two frames).
//!
//! The history only records *decisions*, not poses, so it is cheap to keep around
//! and does not need the database to be rebuilt when a clip changes.

use crate::search::MotionMatch;

/// One past decision: enough to recognise the frame again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistoryEntry {
    /// Index into the database.
    pub frame: usize,
    pub clip: usize,
    /// Where the clip was being started.
    pub time: f32,
}

/// Bounded, newest-last list of recent search results.
#[derive(Debug, Clone)]
pub struct SelectionHistory {
    entries: Vec<HistoryEntry>,
    limit: usize,
}

/// How many decisions are remembered when [`SelectionHistory::default`] is used.
pub const DEFAULT_HISTORY_LIMIT: usize = 8;

impl Default for SelectionHistory {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_LIMIT)
    }
}

impl SelectionHistory {
    /// Keep at most `limit` decisions; `0` records nothing.
    pub fn new(limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            limit,
        }
    }

    /// Record a decision. Recording the frame that is already last only refreshes
    /// its time, so holding the same frame does not fill the history with copies.
    pub fn record(&mut self, selection: &MotionMatch) {
        if self.limit == 0 {
            return;
        }
        let entry = HistoryEntry {
            frame: selection.frame,
            clip: selection.clip,
            time: selection.time,
        };
        if let Some(last) = self.entries.last_mut() {
            if last.frame == entry.frame {
                *last = entry;
                return;
            }
        }
        self.entries.push(entry);
        if self.entries.len() > self.limit {
            let excess = self.entries.len() - self.limit;
            self.entries.drain(..excess);
        }
    }

    /// The frame currently playing, if the search has run at least once.
    pub fn continuing(&self) -> Option<HistoryEntry> {
        self.entries.last().copied()
    }

    /// Every remembered decision, oldest first.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Whether `frame` was picked recently but is not the one still playing.
    pub fn was_reselected_recently(&self, frame: usize) -> bool {
        let Some((last, earlier)) = self.entries.split_last() else {
            return false;
        };
        last.frame != frame && earlier.iter().any(|entry| entry.frame == frame)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Forget everything, e.g. after a teleport or a scene reload.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Cost multipliers applied on top of [`crate::search::MatchWeights`].
///
/// Multipliers keep the raw `pose_cost` / `trajectory_cost` in the returned
/// [`MotionMatch`] untouched, so a debug view can show why a frame was kept
/// without re-running the search.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchBias {
    /// Multiplied into the cost of the frame that is still playing. `1.0` disables
    /// the bonus and makes every decision independent.
    pub continuing: f32,
    /// Multiplied into the cost of frames picked recently (but not the continuing
    /// one). `1.0` disables the penalty.
    pub reselect: f32,
}

impl Default for SearchBias {
    fn default() -> Self {
        Self {
            continuing: 0.85,
            reselect: 1.15,
        }
    }
}

impl SearchBias {
    /// No stickiness at all: every frame competes on its own cost.
    pub const NONE: Self = Self {
        continuing: 1.0,
        reselect: 1.0,
    };

    /// The multiplier `cost` receives if it is the continuing frame or a recent
    /// selection. Non-finite or non-positive values are treated as `1.0` so a bad
    /// config cannot make costs negative.
    pub(crate) fn multiplier(&self, frame: usize, history: &SelectionHistory) -> f32 {
        let continuing = history.continuing().map(|entry| entry.frame);
        if continuing == Some(frame) {
            return sanitize_multiplier(self.continuing);
        }
        if history.was_reselected_recently(frame) {
            return sanitize_multiplier(self.reselect);
        }
        1.0
    }
}

fn sanitize_multiplier(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use crate::search::MotionMatch;

    use super::{SearchBias, SelectionHistory};

    fn matched(frame: usize, cost: f32) -> MotionMatch {
        MotionMatch {
            frame,
            clip: 0,
            time: 0.0,
            pose_cost: cost,
            trajectory_cost: 0.0,
            cost,
        }
    }

    #[test]
    fn recording_the_same_frame_refreshes_instead_of_growing() {
        let mut history = SelectionHistory::new(4);
        history.record(&matched(3, 0.1));
        history.record(&matched(3, 0.1));
        assert_eq!(history.len(), 1);
        assert_eq!(history.continuing().map(|entry| entry.frame), Some(3));
    }

    #[test]
    fn old_decisions_fall_out_of_the_window() {
        let mut history = SelectionHistory::new(2);
        for frame in 0..5 {
            history.record(&matched(frame, 0.1));
        }
        let frames: Vec<usize> = history.entries().iter().map(|entry| entry.frame).collect();
        assert_eq!(frames, vec![3, 4]);
        assert_eq!(history.continuing().map(|entry| entry.frame), Some(4));
    }

    #[test]
    fn the_continuing_frame_is_not_also_a_reselection() {
        let mut history = SelectionHistory::new(4);
        history.record(&matched(1, 0.1));
        history.record(&matched(2, 0.1));
        assert!(!history.was_reselected_recently(2));
        assert!(history.was_reselected_recently(1));
        assert!(!history.was_reselected_recently(9));
    }

    #[test]
    fn multipliers_are_identity_for_unknown_frames_and_bad_values() {
        let mut history = SelectionHistory::new(4);
        history.record(&matched(1, 0.1));
        history.record(&matched(2, 0.1));
        let bias = SearchBias {
            continuing: f32::NAN,
            reselect: -1.0,
        };
        assert_eq!(bias.multiplier(9, &history), 1.0);
        assert_eq!(bias.multiplier(2, &history), 1.0);
        assert_eq!(bias.multiplier(1, &history), 1.0);
        assert_eq!(SearchBias::NONE.multiplier(2, &history), 1.0);
    }

    #[test]
    fn clearing_makes_the_next_search_independent() {
        let mut history = SelectionHistory::new(4);
        history.record(&matched(1, 0.1));
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.continuing(), None);
    }
}
