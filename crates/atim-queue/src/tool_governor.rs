/// Rate-based governor for tool output, with a digest of what it holds back.
///
/// [`crate::chatter_folder::ChatterFolder`] handles a runaway loop that repeats
/// the *same* call — there the identical output is folded into one message with
/// a `xN` counter. This handles the rest: a burst of *distinct* calls, where
/// there is nothing to fold but the chat still fills faster than anyone can
/// read it. It is the backstop for storm shapes nobody anticipated.
///
/// Above the rate limit, tool output is held back and summarised instead of
/// forwarded:
///
/// ```text
/// ⚠️ Tool output is flooding this chat — holding it back and summarising.
///    Replies still come through.
///
/// ⚙️ Held back 47 tool call(s): git status ×20, git log ×20, …
/// ```
///
/// **Assistant text always goes through.** That is the reply the user is
/// waiting for, and this governor is only consulted for tool output.
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// More than this many tool calls inside [`BURST_WINDOW`] trips the governor.
const BURST_LIMIT: usize = 10;
const BURST_WINDOW: Duration = Duration::from_secs(5);

/// …and this many over a longer window trips it even without a sharp burst.
const SUSTAINED_LIMIT: usize = 30;
const SUSTAINED_WINDOW: Duration = Duration::from_secs(60);

/// Tool calls within this window count towards the hard lock threshold.
const LOCK_WINDOW: Duration = Duration::from_secs(300);
/// Past this many calls in [`LOCK_WINDOW`] the run will not end on its own —
/// the user has to speak before tool output resumes.
const LOCK_LIMIT: usize = 200;

/// How long tool calls must stop before a run ends by itself.
const CALM: Duration = Duration::from_secs(30);
/// How often to summarise what is being held back while a run lasts, so a storm
/// gets one digest every so often instead of one per held call.
const DIGEST_INTERVAL: Duration = Duration::from_secs(10);
/// How many of the busiest labels to name in a digest before moving on.
const DIGEST_ROWS: usize = 5;

/// Bounds so a long session cannot grow the map without limit.
const MAX_RECENT: usize = 1024;
const MAX_HELD_USES: usize = 4096;

/// What to do with one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDecision {
    /// Forward it normally.
    Forward,
    /// Hold it back — it is part of a run filling the chat.
    Hold,
}

/// Something worth telling the chat about the tool-output policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StormNotice {
    /// A run just started; tool output is being held back.
    Began,
    /// Periodic summary of what is being held back.
    Digest(Vec<(String, usize)>),
    /// The run ended — report what never went out.
    Ended(Vec<(String, usize)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    /// Ends on its own once tool calls stop for [`CALM`].
    Quiet,
    /// Too long a storm to end unattended; the user has to speak first.
    Locked,
}

/// Per-chat run state.
#[derive(Default)]
struct ChatState {
    /// Recent tool-call times, oldest first, covering [`LOCK_WINDOW`].
    recent: VecDeque<Instant>,
    /// Whether tool output is held back, and how the run may end.
    run: Option<RunKind>,
    /// Label -> count of tool calls held back in the current run.
    held: HashMap<String, usize>,
    /// Total tool calls held back in the current run.
    held_total: usize,
    /// `tool_use_id`s whose call was held, so its result is held with it.
    held_uses: HashSet<String>,
    /// When a digest was last written.
    last_digest: Option<Instant>,
}

/// Holds back tool output when a chat is being flooded, per chat.
#[derive(Default)]
pub struct ToolStormGuard {
    states: HashMap<(i64, i64), ChatState>,
}

impl ToolStormGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one tool call and decide whether to forward it.
    ///
    /// `label` names the call (the command, the file) and is what the digest
    /// summarises. `use_id` ties the result to its call so both halves are
    /// treated the same.
    pub fn observe(
        &mut self,
        chat: (i64, i64),
        label: &str,
        use_id: Option<&str>,
        now: Instant,
    ) -> (ToolDecision, Option<StormNotice>) {
        let state = self.states.entry(chat).or_default();
        Self::note_call(state, now);

        let long = state.recent.len();
        let sustained = count_since(&state.recent, now, SUSTAINED_WINDOW);
        let burst = count_since(&state.recent, now, BURST_WINDOW);

        let just_began = match state.run {
            Some(RunKind::Locked) => false,
            Some(RunKind::Quiet) => {
                // A storm this long should not end while it is still going.
                if long > LOCK_LIMIT {
                    state.run = Some(RunKind::Locked);
                }
                false
            }
            None => {
                if burst > BURST_LIMIT || sustained > SUSTAINED_LIMIT {
                    state.run = Some(if long > LOCK_LIMIT {
                        RunKind::Locked
                    } else {
                        RunKind::Quiet
                    });
                    true
                } else {
                    return (ToolDecision::Forward, None);
                }
            }
        };

        Self::hold(state, label, use_id, now, just_began)
    }

    /// Whether a tool result's call was held back, so the result goes with it.
    ///
    /// Forwarding the result of a held call would post output with no call in
    /// front of it. Clears the marker either way.
    pub fn take_held_result(&mut self, chat: (i64, i64), use_id: &str) -> bool {
        self.states
            .get_mut(&chat)
            .is_some_and(|s| s.held_uses.remove(use_id))
    }

    /// End any run for `chat` because the user spoke.
    ///
    /// A locked run ends only this way, which is what makes it safe to hold
    /// tool output for as long as a storm lasts.
    pub fn note_inbound(&mut self, chat: (i64, i64)) -> Option<StormNotice> {
        Self::end_run(self.states.get_mut(&chat)?)
    }

    /// Time-based upkeep for every chat: write digests, and end runs that have
    /// gone quiet. Driven by the delivery task's tick.
    ///
    /// A [`RunKind::Locked`] run deliberately survives any amount of calm.
    pub fn poll_all(&mut self, now: Instant) -> Vec<((i64, i64), StormNotice)> {
        let mut out = Vec::new();
        for (chat, state) in self.states.iter_mut() {
            if state.run.is_none() {
                continue;
            }
            let calm = state
                .recent
                .back()
                .is_none_or(|t| now.saturating_duration_since(*t) >= CALM);
            if calm {
                // A quiet run ends on its own; a locked one waits for the user.
                // Either way a digest now would only repeat itself — the storm
                // has stopped, so there is nothing new to summarise.
                if state.run == Some(RunKind::Quiet)
                    && let Some(notice) = Self::end_run(state)
                {
                    out.push((*chat, notice));
                }
                continue;
            }
            let due = state
                .last_digest
                .is_none_or(|t| now.saturating_duration_since(t) >= DIGEST_INTERVAL);
            if due && state.held_total > 0 {
                state.last_digest = Some(now);
                out.push((*chat, StormNotice::Digest(summarise(&state.held))));
            }
        }
        out
    }

    /// Record a tool call in the rate windows.
    fn note_call(state: &mut ChatState, now: Instant) {
        state.recent.push_back(now);
        while state
            .recent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > LOCK_WINDOW)
        {
            state.recent.pop_front();
        }
        // The window is also size-bounded: a single batch can carry hundreds of
        // calls sharing one timestamp, which no amount of pruning by age removes.
        while state.recent.len() > MAX_RECENT {
            state.recent.pop_front();
        }
    }

    /// Record a held call. `announcing` marks the one that started the run.
    fn hold(
        state: &mut ChatState,
        label: &str,
        use_id: Option<&str>,
        now: Instant,
        announcing: bool,
    ) -> (ToolDecision, Option<StormNotice>) {
        *state.held.entry(label.to_string()).or_insert(0) += 1;
        state.held_total += 1;
        state.last_digest = Some(state.last_digest.unwrap_or(now));
        if let Some(id) = use_id
            && state.held_uses.len() < MAX_HELD_USES
        {
            state.held_uses.insert(id.to_string());
        }
        (
            ToolDecision::Hold,
            if announcing {
                Some(StormNotice::Began)
            } else {
                None
            },
        )
    }

    /// Close the run on `state` and report what never went out.
    ///
    /// The rate windows go with it. Without that, the calls that started the
    /// run would still be counted and the very next one would start another —
    /// so speaking up would buy the user nothing, and a locked run would have
    /// no way out at all.
    fn end_run(state: &mut ChatState) -> Option<StormNotice> {
        state.run.take()?;
        state.recent.clear();
        state.held_uses.clear();
        state.last_digest = None;
        let held = std::mem::take(&mut state.held);
        let total = std::mem::take(&mut state.held_total);
        if total > 0 {
            Some(StormNotice::Ended(summarise(&held)))
        } else {
            None
        }
    }
}

/// Count entries of `recent` no older than `window`.
fn count_since(recent: &VecDeque<Instant>, now: Instant, window: Duration) -> usize {
    recent
        .iter()
        .filter(|t| now.saturating_duration_since(**t) <= window)
        .count()
}

/// Busiest labels first, capped so a thousand distinct commands stay readable.
fn summarise(held: &HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut rows: Vec<(String, usize)> = held
        .iter()
        .map(|(label, count)| (label.clone(), *count))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(DIGEST_ROWS);
    rows
}

/// Short name for a digest row: the command for a shell call, the path for a
/// file call, whatever identifies the call otherwise.
///
/// Grouping on this is what turns 47 held calls into "git status ×20, git log
/// ×20, …" instead of 47 lines. Collisions across tools are acceptable here —
/// this is a summary, not an audit log.
pub fn label_from_summary(summary: &str) -> String {
    // A fenced block carries the call's actual payload (a shell command).
    if let Some(start) = summary.find("```") {
        let rest = &summary[start + 3..];
        let rest = rest.strip_prefix("bash").unwrap_or(rest);
        if let Some(end) = rest.find("```") {
            let body = rest[..end].trim();
            if !body.is_empty() {
                return first_line(body);
            }
        }
    }

    // Otherwise drop the "💻 Bash: " prefix and keep what identifies the call.
    match summary.split_once(": ") {
        Some((_, detail)) if !detail.trim().is_empty() => first_line(detail.trim()),
        // Nothing identifying (bare "🔧 Tool") — the name is all there is.
        _ => first_line(summary.split(':').next().unwrap_or(summary).trim()),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_string()
}

/// Chat-ready wording for a [`StormNotice`].
pub fn render(notice: &StormNotice) -> String {
    match notice {
        StormNotice::Began => {
            "⚠️ Tool output is flooding this chat — holding it back and summarising. \
             Replies still come through."
                .to_string()
        }
        StormNotice::Digest(rows) => format!("⚙️ {}", held_line(rows)),
        StormNotice::Ended(rows) => format!("✅ Tool output resumed. {}", held_line(rows)),
    }
}

fn held_line(rows: &[(String, usize)]) -> String {
    let listed: Vec<String> = rows
        .iter()
        .map(|(label, count)| format!("{label} ×{count}"))
        .collect();
    format!(
        "Held back {} tool call(s): {}",
        rows.iter().map(|(_, c)| c).sum::<usize>(),
        listed.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: (i64, i64) = (1, 2);
    /// Calls packed 10ms apart — far inside [`BURST_WINDOW`], so a handful of
    /// them is a burst and the same count spread over minutes is not.
    const STEP: Duration = Duration::from_millis(10);

    /// Feed `n` tightly packed calls; returns every decision and notice.
    fn burst(
        guard: &mut ToolStormGuard,
        t0: Instant,
        n: usize,
    ) -> Vec<(ToolDecision, Option<StormNotice>)> {
        (0..n)
            .map(|i| guard.observe(CHAT, "git status", None, t0 + STEP * i as u32))
            .collect()
    }

    #[test]
    fn test_quiet_traffic_is_forwarded() {
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        for i in 0..BURST_LIMIT {
            let (decision, notice) = guard.observe(CHAT, "git status", None, t0 + STEP * i as u32);
            assert_eq!(decision, ToolDecision::Forward);
            assert_eq!(notice, None);
        }
    }

    #[test]
    fn test_a_burst_announces_the_run_exactly_once() {
        let mut guard = ToolStormGuard::new();
        let began = burst(&mut guard, Instant::now(), BURST_LIMIT + 20)
            .iter()
            .filter(|(_, n)| *n == Some(StormNotice::Began))
            .count();
        assert_eq!(began, 1);
    }

    #[test]
    fn test_the_whole_tail_of_a_burst_is_held() {
        let mut guard = ToolStormGuard::new();
        let held = burst(&mut guard, Instant::now(), BURST_LIMIT + 20)
            .iter()
            .filter(|(d, _)| *d == ToolDecision::Hold)
            .count();
        assert!(held >= 20, "expected the tail held, got {held}");
    }

    #[test]
    fn test_a_slow_trickle_never_trips_it() {
        // The same total volume, spread past the sustained window: not a storm.
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        for i in 0..SUSTAINED_LIMIT {
            let step = Duration::from_secs(120) * i as u32;
            assert_eq!(
                guard.observe(CHAT, "git status", None, t0 + step).0,
                ToolDecision::Forward
            );
        }
    }

    #[test]
    fn test_a_dense_minute_trips_it_without_a_burst() {
        // Two seconds apart: never a burst, but the minute window fills up.
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        let held = (0..SUSTAINED_LIMIT + 5)
            .filter(|i| {
                guard
                    .observe(
                        CHAT,
                        "git status",
                        None,
                        t0 + Duration::from_secs(2 * *i as u64),
                    )
                    .0
                    == ToolDecision::Hold
            })
            .count();
        assert!(held > 0, "a dense minute must trip the governor");
    }

    #[test]
    fn test_a_held_call_takes_its_result_with_it() {
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        for i in 0..BURST_LIMIT {
            guard.observe(CHAT, "cmd", Some(&format!("u{i}")), t0 + STEP * i as u32);
        }
        guard.observe(CHAT, "cmd", Some("held-1"), t0 + STEP * BURST_LIMIT as u32);

        assert!(guard.take_held_result(CHAT, "held-1"));
        assert!(
            !guard.take_held_result(CHAT, "held-1"),
            "the marker is consumed"
        );
        // A forwarded call's result still goes out.
        assert!(!guard.take_held_result(CHAT, "u0"));
    }

    #[test]
    fn test_a_run_ends_after_it_goes_quiet() {
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        burst(&mut guard, t0, BURST_LIMIT + 5);
        let last_call = t0 + STEP * (BURST_LIMIT + 4) as u32;

        // Still active — nothing to report.
        assert!(guard.poll_all(last_call).is_empty());

        let notices = guard.poll_all(last_call + CALM + Duration::from_secs(1));
        assert_eq!(notices.len(), 1);
        assert!(matches!(notices[0].1, StormNotice::Ended(_)));
    }

    #[test]
    fn test_the_ended_notice_counts_what_was_held() {
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        burst(&mut guard, t0, BURST_LIMIT + 3);

        let Some(StormNotice::Ended(rows)) = guard.note_inbound(CHAT) else {
            panic!("expected an Ended notice");
        };
        assert_eq!(rows[0].0, "git status");
        assert!(rows[0].1 >= 3, "should report the held count");
    }

    #[test]
    fn test_a_user_message_ends_a_run_early() {
        let mut guard = ToolStormGuard::new();
        burst(&mut guard, Instant::now(), BURST_LIMIT + 3);
        assert!(matches!(
            guard.note_inbound(CHAT),
            Some(StormNotice::Ended(_))
        ));
        // And tool output flows again.
        assert_eq!(
            guard.observe(CHAT, "cmd", None, Instant::now()).0,
            ToolDecision::Forward
        );
    }

    #[test]
    fn test_a_long_storm_needs_the_user_to_end_it() {
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        for i in 0..(LOCK_LIMIT + 50) {
            guard.observe(CHAT, "cmd", None, t0 + STEP * i as u32);
        }

        // Long past any calm window, but it is locked so it does not end.
        let far_future = t0 + Duration::from_secs(3600);
        assert!(guard.poll_all(far_future).is_empty());

        // The user speaks, and it does.
        assert!(matches!(
            guard.note_inbound(CHAT),
            Some(StormNotice::Ended(_))
        ));
    }

    #[test]
    fn test_digests_are_throttled_not_one_per_call() {
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        burst(&mut guard, t0, BURST_LIMIT + 2);
        let began = t0 + STEP * BURST_LIMIT as u32;

        // A moment later nothing is due: a storm must not emit one digest per
        // held call.
        assert!(guard.poll_all(began + Duration::from_secs(1)).is_empty());

        // One digest once the interval has passed …
        let due = guard.poll_all(began + DIGEST_INTERVAL);
        assert!(matches!(
            due.first().map(|n| &n.1),
            Some(StormNotice::Digest(_))
        ));
        // … and not again straight after.
        assert!(
            guard
                .poll_all(began + DIGEST_INTERVAL + Duration::from_secs(1))
                .is_empty()
        );
    }

    #[test]
    fn test_digest_names_the_busiest_labels() {
        let mut held = HashMap::new();
        held.insert("git status".to_string(), 20);
        held.insert("git log".to_string(), 20);
        held.insert("rare".to_string(), 1);
        let rows = summarise(&held);
        assert_eq!(rows[0].1, 20);
        assert_eq!(rows[1].1, 20);
        assert_eq!(rows[2], ("rare".to_string(), 1));
    }

    #[test]
    fn test_digest_is_capped_so_many_commands_stay_readable() {
        let mut held = HashMap::new();
        for i in 0..500 {
            held.insert(format!("cmd-{i}"), i);
        }
        assert_eq!(summarise(&held).len(), DIGEST_ROWS);
    }

    #[test]
    fn test_rendered_digest_matches_the_shape_users_see() {
        let text = render(&StormNotice::Digest(vec![
            ("git status".to_string(), 20),
            ("git log".to_string(), 7),
        ]));
        assert!(text.contains("Held back 27 tool call(s)"));
        assert!(text.contains("git status ×20"));
        assert!(text.contains("git log ×7"));
    }

    #[test]
    fn test_state_stays_bounded() {
        let mut guard = ToolStormGuard::new();
        let t0 = Instant::now();
        for i in 0..(MAX_RECENT * 2) {
            guard.observe(CHAT, "cmd", Some(&format!("u{i}")), t0 + STEP * i as u32);
        }
        let state = &guard.states[&CHAT];
        assert!(state.recent.len() <= MAX_RECENT);
        assert!(state.held_uses.len() <= MAX_HELD_USES);
    }

    // ── digest labels ──

    #[test]
    fn test_label_is_the_command_for_a_shell_call() {
        let summary = "💻 Bash:\n```bash\ngit status\n```";
        assert_eq!(label_from_summary(summary), "git status");
    }

    #[test]
    fn test_label_is_the_path_for_a_file_call() {
        assert_eq!(
            label_from_summary("📖 Read: /home/me/src/main.rs"),
            "/home/me/src/main.rs"
        );
    }

    #[test]
    fn test_label_keeps_the_first_line_of_a_multiline_payload() {
        let summary = "💻 Bash:\n```bash\ncargo build --release\necho done\n```";
        assert_eq!(label_from_summary(summary), "cargo build --release");
    }

    #[test]
    fn test_label_falls_back_to_the_tool_name() {
        assert_eq!(label_from_summary("🔧 WebFetch"), "🔧 WebFetch");
    }
}
