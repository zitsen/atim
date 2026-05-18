use aim_core::message::{MessageTarget, WindowId};
use aim_core::session::ThreadBinding;

/// Routes IM events to tmux windows based on thread bindings.
pub struct Router {
    bindings: Vec<ThreadBinding>,
}

impl Router {
    pub fn new(bindings: Vec<ThreadBinding>) -> Self {
        Self { bindings }
    }

    /// Find the window bound to a given IM target.
    pub fn resolve_window(&self, target: &MessageTarget) -> Option<&WindowId> {
        let thread_id = target.thread_id.map(|t| t.0).unwrap_or(0);
        self.bindings
            .iter()
            .find(|b| b.thread_id == thread_id)
            .map(|b| {
                // Return a reference to a WindowId
                // We store it as a string in the binding
                let _ = &b.window_id;
                // This is a placeholder — real resolution uses window_id
            });
        None
    }
}
