use std::collections::HashMap;

use atim_core::message::{MessageTarget, WindowId};
use atim_core::session::{ChatBinding, WindowBinding};

/// Routes IM events to tmux windows based on chat bindings.
///
/// Currently unused — the Server struct does resolution inline.
/// Kept as a stable API for future refactoring.
#[allow(dead_code)]
pub struct Router {
    bindings: Vec<ChatBinding>,
    windows: HashMap<String, WindowBinding>,
}

#[allow(dead_code)]
impl Router {
    pub fn new(bindings: Vec<ChatBinding>, windows: HashMap<String, WindowBinding>) -> Self {
        Self { bindings, windows }
    }

    /// Find the window bound to a given IM target.
    pub fn resolve_window(&self, target: &MessageTarget) -> Option<WindowId> {
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        let cb = self.bindings.iter().find(|b| b.thread_id == thread_id)?;
        if cb.session_id.is_empty() {
            return None;
        }
        self.windows
            .values()
            .find(|wb| wb.session_id == cb.session_id)
            .map(|wb| WindowId(wb.window_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atim_core::message::{ChatId, ThreadId};

    #[test]
    fn test_resolve_window_finds_binding() {
        let mut windows = HashMap::new();
        windows.insert(
            "@0".into(),
            WindowBinding {
                window_id: "@0".into(),
                session_id: "sess-abc".into(),
                cwd: "/".into(),
                agent_type: "claude".into(),
                window_name: "test".into(),
            },
        );
        let router = Router::new(
            vec![ChatBinding {
                user_id: 1,
                thread_id: 100,
                chat_id: -100,
                display_name: "test".into(),
                group_chat_id: None,
                topic_name: None,
                session_id: "sess-abc".into(),
                reply_at_only: false,
            }],
            windows,
        );

        let target = MessageTarget {
            chat_id: ChatId(-100),
            thread_id: Some(ThreadId(100)),
            chat_name: None,
        };

        let result = router.resolve_window(&target);
        assert_eq!(result, Some(WindowId("@0".into())));
    }

    #[test]
    fn test_resolve_window_returns_none_for_unknown() {
        let router = Router::new(vec![], HashMap::new());

        let target = MessageTarget {
            chat_id: ChatId(-200),
            thread_id: Some(ThreadId(999)),
            chat_name: None,
        };

        assert_eq!(router.resolve_window(&target), None);
    }

    #[test]
    fn test_resolve_window_returns_none_when_no_window() {
        let router = Router::new(
            vec![ChatBinding {
                user_id: 1,
                thread_id: 55,
                chat_id: -100,
                display_name: "win1".into(),
                group_chat_id: None,
                topic_name: None,
                session_id: "sess-orphan".into(),
                reply_at_only: false,
            }],
            HashMap::new(),
        );

        let target = MessageTarget {
            chat_id: ChatId(-100),
            thread_id: Some(ThreadId(55)),
            chat_name: None,
        };

        assert_eq!(router.resolve_window(&target), None);
    }
}
