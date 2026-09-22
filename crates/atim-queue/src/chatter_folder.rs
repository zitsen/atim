/// Collapses repeated identical tool calls into one message with a counter.
///
/// A runaway tool loop emits the same call hundreds of times over — the same
/// `git status` a thousand times, each with the same output. Forwarding each
/// one buries the chat in noise the user has to scroll past. Folding them into
/// one message with a `×N` counter says what happened without the spam:
///
/// ```text
/// ✅ Bash: git status — 位于分支 main / 工作区干净
/// └─ 相同输出已重复 ×1301
/// ```
///
/// Two calls only fold together when *both* the call and its output match: a
/// `git status` whose result changed is new information and is shown normally.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use atim_core::message::MessageId;

/// How many distinct fold groups to keep alive.
///
/// Groups are cheap but not free, and a long session can touch unbounded
/// distinct calls. Beyond this the quietest group is dropped — its message is
/// already on the chat, so only further folding is lost.
const DEFAULT_CAP: usize = 256;

/// How long a group may sit with an out-of-date counter before the true total
/// is written out.
///
/// The counter is also refreshed on a doubling schedule (×2, ×4, ×8 …), which
/// costs about ten edits across a thousand repeats. This is the backstop that
/// closes the gap once a run goes quiet, so the chat is not left showing
/// `×1024` for what was really `×1301`.
///
/// Public so the caller's idle tick cannot drift out of step with it.
pub const IDLE_FLUSH_SECS: u64 = 30;
const IDLE_FLUSH: Duration = Duration::from_secs(IDLE_FLUSH_SECS);

/// Identifies "this exact call produced this exact output".
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FoldKey {
    /// (chat_id, thread_id) — the same call in two chats is not a repeat.
    chat: (i64, i64),
    tool: String,
    use_text: String,
    result_text: String,
}

impl FoldKey {
    pub fn new(
        chat: (i64, i64),
        tool_name: Option<&str>,
        use_text: &str,
        result_text: &str,
    ) -> Self {
        Self {
            chat,
            tool: tool_name.unwrap_or("tool").to_string(),
            use_text: use_text.to_string(),
            result_text: result_text.to_string(),
        }
    }
}

/// What the caller should do with a run of identical calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldPlan {
    /// Show the first of them and fold the rest.
    ShowFirst,
    /// Fold all of them — this call is already on the chat.
    FoldAll,
}

/// A message whose repeat counter has fallen behind and needs rewriting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterUpdate {
    /// `(chat_id, thread_id)` the message lives in.
    pub chat: (i64, i64),
    pub msg_id: MessageId,
    pub text: String,
}

/// One message standing in for N identical calls.
struct FoldGroup {
    /// Message showing the call and its output. `None` while the anchor's send
    /// has not landed yet (it may have been shed by flood control).
    msg_id: Option<MessageId>,
    /// Text the message shows without the counter.
    body: String,
    /// How many identical calls have been observed.
    count: usize,
    /// Highest count already written to the message (`1` = no counter shown).
    shown: usize,
    /// When `count` last changed — drives [`ChatterFolder::flush_stale`].
    last_bump: Instant,
}

impl FoldGroup {
    /// Text the message should show for `count` calls.
    fn render(&self) -> String {
        if self.count <= 1 {
            return self.body.clone();
        }
        format!("{}\n└─ 相同输出已重复 ×{}", self.body, self.count)
    }
}

/// Folds repeated identical tool calls, per chat.
pub struct ChatterFolder {
    groups: HashMap<FoldKey, FoldGroup>,
    cap: usize,
}

impl ChatterFolder {
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_CAP)
    }

    pub fn with_cap(cap: usize) -> Self {
        Self {
            groups: HashMap::new(),
            cap: cap.max(1),
        }
    }

    /// Note `n` occurrences of `key` in the batch being processed.
    ///
    /// `ShowFirst` is returned both for a first sighting and when a previous
    /// anchor never made it to the chat — otherwise a shed anchor would fold a
    /// thousand calls into a message nobody can see.
    pub fn observe(&mut self, key: &FoldKey, n: usize) -> FoldPlan {
        let now = Instant::now();
        let group = match self.groups.get_mut(key) {
            Some(group) => group,
            None => {
                self.evict_if_full();
                self.groups.insert(
                    key.clone(),
                    FoldGroup {
                        msg_id: None,
                        body: String::new(),
                        count: n,
                        shown: 1,
                        last_bump: now,
                    },
                );
                return FoldPlan::ShowFirst;
            }
        };

        group.count += n;
        group.last_bump = now;
        if group.msg_id.is_some() {
            FoldPlan::FoldAll
        } else {
            FoldPlan::ShowFirst
        }
    }

    /// Attach the message that now displays `key`.
    pub fn bind(&mut self, key: &FoldKey, msg_id: MessageId, body: String) {
        if let Some(group) = self.groups.get_mut(key) {
            group.msg_id = Some(msg_id);
            group.body = body;
        }
    }

    /// New text for `key`'s message, if its counter has fallen far enough
    /// behind to be worth an edit.
    ///
    /// The counter doubles what is shown each time (×2, ×4, ×8 …), so a
    /// thousand repeats cost about ten edits instead of a thousand.
    pub fn refresh(&mut self, key: &FoldKey) -> Option<CounterUpdate> {
        let group = self.groups.get_mut(key)?;
        let msg_id = group.msg_id.clone()?;
        if group.count < group.shown * 2 {
            return None;
        }
        group.shown = group.count;
        Some(CounterUpdate {
            chat: key.chat,
            msg_id,
            text: group.render(),
        })
    }

    /// Groups whose counter is behind the message after going quiet.
    ///
    /// Doubling leaves the display at most a factor of two stale; this closes
    /// that gap for runs that have stopped, so the chat ends up showing the
    /// true total rather than `×1024` for `×1301`.
    pub fn flush_stale(&mut self) -> Vec<CounterUpdate> {
        let now = Instant::now();
        let mut out = Vec::new();
        for (key, group) in self.groups.iter_mut() {
            let Some(msg_id) = group.msg_id.clone() else {
                continue;
            };
            if group.count <= group.shown {
                continue;
            }
            if now.duration_since(group.last_bump) < IDLE_FLUSH {
                continue;
            }
            group.shown = group.count;
            out.push(CounterUpdate {
                chat: key.chat,
                msg_id,
                text: group.render(),
            });
        }
        out
    }

    /// Drop the quietest group to make room, keeping the map bounded.
    fn evict_if_full(&mut self) {
        if self.groups.len() < self.cap {
            return;
        }
        if let Some(oldest) = self
            .groups
            .iter()
            .min_by_key(|(_, g)| g.last_bump)
            .map(|(k, _)| k.clone())
        {
            self.groups.remove(&oldest);
        }
    }
}

impl Default for ChatterFolder {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether repeated calls to this tool may be folded.
///
/// Edits and file writes are worth showing one by one — the user is reviewing
/// what changed. Interactive prompts need an answer and must not be hidden.
/// Everything else (a thousand identical `git status` runs) is fair game.
pub fn foldable(tool_name: Option<&str>) -> bool {
    !matches!(
        tool_name,
        Some(
            "AskUserQuestion"
                | "Edit"
                | "EditTool"
                | "TextEditTool"
                | "Write"
                | "WriteTool"
                | "NotebookEdit"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::SubAssign;

    fn key(use_text: &str, result_text: &str) -> FoldKey {
        FoldKey::new((1, 2), Some("Bash"), use_text, result_text)
    }

    fn id(n: u64) -> MessageId {
        MessageId(format!("mock:{n}"))
    }

    /// Fold five identical calls under one bound message and report what the
    /// message should say.
    fn fold_five() -> (ChatterFolder, FoldKey) {
        let mut folder = ChatterFolder::new();
        let k = key("git status", "clean");
        assert_eq!(folder.observe(&k, 5), FoldPlan::ShowFirst);
        folder.bind(&k, id(1), "✅ Bash: git status (clean)".to_string());
        (folder, k)
    }

    #[test]
    fn test_first_sighting_is_shown() {
        let mut folder = ChatterFolder::new();
        assert_eq!(folder.observe(&key("a", "b"), 1), FoldPlan::ShowFirst);
    }

    #[test]
    fn test_identical_calls_fold_once_shown() {
        let (mut folder, k) = fold_five();
        assert_eq!(folder.observe(&k, 3), FoldPlan::FoldAll);
    }

    #[test]
    fn test_differing_output_is_not_a_repeat() {
        // Same `git status`, different output — new information, show it.
        let mut folder = ChatterFolder::new();
        assert_eq!(
            folder.observe(&key("git status", "clean"), 1),
            FoldPlan::ShowFirst
        );
        assert_eq!(
            folder.observe(&key("git status", "3 files changed"), 1),
            FoldPlan::ShowFirst
        );
    }

    #[test]
    fn test_same_call_in_another_chat_is_not_a_repeat() {
        let mut folder = ChatterFolder::new();
        let ours = key("git status", "clean");
        assert_eq!(folder.observe(&ours, 1), FoldPlan::ShowFirst);
        let theirs = FoldKey::new((9, 9), Some("Bash"), "git status", "clean");
        assert_eq!(folder.observe(&theirs, 1), FoldPlan::ShowFirst);
    }

    #[test]
    fn test_unbound_group_shows_again() {
        // Regression: if the anchor's send was shed by flood control there is no
        // message to fold into, so the next sighting must be shown. Otherwise a
        // thousand calls would collapse into nothing at all.
        let mut folder = ChatterFolder::new();
        let k = key("git status", "clean");
        assert_eq!(folder.observe(&k, 4), FoldPlan::ShowFirst);
        // No `bind` — the anchor never landed.
        assert_eq!(folder.observe(&k, 4), FoldPlan::ShowFirst);
        folder.bind(&k, id(1), "body".to_string());
        assert_eq!(folder.observe(&k, 4), FoldPlan::FoldAll);
    }

    #[test]
    fn test_counter_shows_total_across_unbound_runs() {
        let mut folder = ChatterFolder::new();
        let k = key("git status", "clean");
        folder.observe(&k, 4);
        folder.observe(&k, 4);
        folder.bind(&k, id(1), "body".to_string());
        assert_eq!(
            folder.refresh(&k).unwrap().text,
            "body\n└─ 相同输出已重复 ×8"
        );
    }

    #[test]
    fn test_single_call_shows_no_counter() {
        let mut folder = ChatterFolder::new();
        let k = key("git status", "clean");
        folder.observe(&k, 1);
        folder.bind(&k, id(1), "body".to_string());
        assert_eq!(folder.refresh(&k), None);
    }

    #[test]
    fn test_counter_refreshes_on_a_doubling_schedule() {
        // A thousand repeats must not cost a thousand edits.
        let (mut folder, k) = fold_five();
        let mut edits = 0;

        // ×5 is more than double what is shown (1), so it is written out …
        if folder.refresh(&k).is_some() {
            edits += 1;
        }
        // … and then only at ×8, ×16, ×32 … up to ×1024.
        for _ in 0..995 {
            folder.observe(&k, 1);
            if folder.refresh(&k).is_some() {
                edits += 1;
            }
        }

        assert!(edits <= 11, "doubling should cost ~10 edits, got {edits}");
    }

    #[test]
    fn test_flush_stale_writes_the_true_total_after_a_pause() {
        let (mut folder, k) = fold_five();
        // Cross a doubling so the display moves to ×5.
        folder.refresh(&k).unwrap();
        folder.observe(&k, 1);
        assert_eq!(folder.refresh(&k), None, "×6 is not a doubling of ×5");

        // Not idle yet — the display may lag.
        assert!(folder.flush_stale().is_empty());

        // Age the group past IDLE_FLUSH and the true total must land.
        folder
            .groups
            .get_mut(&k)
            .unwrap()
            .last_bump
            .sub_assign(IDLE_FLUSH + Duration::from_secs(1));
        let flushed = folder.flush_stale();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].chat, (1, 2));
        assert_eq!(
            flushed[0].text,
            "✅ Bash: git status (clean)\n└─ 相同输出已重复 ×6"
        );
    }

    #[test]
    fn test_flush_stale_ignores_groups_already_in_sync() {
        let (mut folder, k) = fold_five();
        folder.refresh(&k).unwrap();
        folder
            .groups
            .get_mut(&k)
            .unwrap()
            .last_bump
            .sub_assign(IDLE_FLUSH + Duration::from_secs(1));
        assert!(folder.flush_stale().is_empty());
    }

    #[test]
    fn test_map_stays_bounded_by_dropping_the_quietest_group() {
        let mut folder = ChatterFolder::with_cap(3);
        for i in 0..10 {
            folder.observe(&key(&format!("cmd-{i}"), "out"), 1);
        }
        assert!(folder.groups.len() <= 3);
    }

    #[test]
    fn test_edits_and_writes_are_never_foldable() {
        for tool in [
            "AskUserQuestion",
            "Edit",
            "EditTool",
            "TextEditTool",
            "Write",
            "WriteTool",
            "NotebookEdit",
        ] {
            assert!(!foldable(Some(tool)), "{tool} must never be folded");
        }
        assert!(foldable(Some("Bash")));
        assert!(foldable(Some("Read")));
        assert!(foldable(None));
    }
}
