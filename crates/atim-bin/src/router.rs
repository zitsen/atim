use atim_core::message::{MessageTarget, WindowId};
use atim_core::session::ThreadBinding;

/// Routes IM events to tmux windows based on thread bindings.
///
/// Currently unused — the Server struct does resolution inline.
/// Kept as a stable API for future refactoring.
#[allow(dead_code)]
pub struct Router {
    bindings: Vec<ThreadBinding>,
}

#[allow(dead_code)]
impl Router {
    pub fn new(bindings: Vec<ThreadBinding>) -> Self {
        Self { bindings }
    }

    /// Find the window bound to a given IM target.
    pub fn resolve_window(&self, target: &MessageTarget) -> Option<WindowId> {
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        self.bindings
            .iter()
            .find(|b| b.thread_id == thread_id)
            .map(|b| WindowId(b.window_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atim_core::message::{ChatId, ThreadId};

    #[test]
    fn test_resolve_window_finds_binding() {
        let router = Router::new(vec![
            ThreadBinding {
                user_id: 1,
                thread_id: 100,
                chat_id: -100,
                window_id: "@0".into(),
                display_name: "test".into(),
                group_chat_id: None,
                topic_name: None,
            },
        ]);

        let target = MessageTarget {
            chat_id: ChatId(-100),
            thread_id: Some(ThreadId(100)),
        };

        let result = router.resolve_window(&target);
        assert_eq!(result, Some(WindowId("@0".into())));
    }

    #[test]
    fn test_resolve_window_returns_none_for_unknown() {
        let router = Router::new(vec![]);

        let target = MessageTarget {
            chat_id: ChatId(-200),
            thread_id: Some(ThreadId(999)),
        };

        assert_eq!(router.resolve_window(&target), None);
    }

    #[test]
    fn test_resolve_window_matches_by_thread_id_only() {
        let router = Router::new(vec![
            ThreadBinding {
                user_id: 1,
                thread_id: 55,
                chat_id: -100,
                window_id: "@1".into(),
                display_name: "win1".into(),
                group_chat_id: None,
                topic_name: None,
            },
            ThreadBinding {
                user_id: 2,
                thread_id: 55,
                chat_id: -200,
                window_id: "@2".into(),
                display_name: "win2".into(),
                group_chat_id: None,
                topic_name: None,
            },
        ]);

        let target = MessageTarget {
            chat_id: ChatId(-100),
            thread_id: Some(ThreadId(55)),
        };

        // Returns the first binding with matching thread_id
        let result = router.resolve_window(&target);
        assert_eq!(result, Some(WindowId("@1".into())));
    }
}
