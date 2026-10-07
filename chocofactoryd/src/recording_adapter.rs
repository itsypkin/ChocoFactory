//! Test-only adapter that records the calls it receives (#166).

/// One call a [`RecordingAdapter`] saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordedCall {
    Start,
    Resume { adapter_session_id: String },
}

/// A test adapter under a chosen name that delegates to a real
/// `ClaudeAdapter` (on a fixture binary), so turns complete normally, and
/// records every call it receives.
pub(crate) struct RecordingAdapter {
    name: &'static str,
    inner: crate::adapter::ClaudeAdapter,
    pub(crate) calls: std::sync::Arc<std::sync::Mutex<Vec<RecordedCall>>>,
}

impl RecordingAdapter {
    pub(crate) fn new(name: &'static str, binary: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            name,
            inner: crate::adapter::ClaudeAdapter::with_binary(binary),
            calls: Default::default(),
        })
    }

    pub(crate) fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl crate::adapter::AgentAdapter for RecordingAdapter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn start(
        &self,
        prompt: &str,
        cfg: &crate::adapter::RoleConfig,
    ) -> Result<crate::adapter::AgentHandle, crate::adapter::AdapterError> {
        self.calls.lock().unwrap().push(RecordedCall::Start);
        self.inner.start(prompt, cfg)
    }

    fn resume(
        &self,
        session_id: &str,
        prompt: &str,
        cfg: &crate::adapter::RoleConfig,
    ) -> Result<crate::adapter::AgentHandle, crate::adapter::AdapterError> {
        self.calls.lock().unwrap().push(RecordedCall::Resume {
            adapter_session_id: session_id.to_string(),
        });
        self.inner.resume(session_id, prompt, cfg)
    }
}
