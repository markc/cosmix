use tokio::sync::broadcast;

const DEFAULT_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum DaemonEvent {
    WindowOpened { app_id: String, title: String },
    WindowClosed { app_id: String },
    WindowFocused { app_id: String },
    WorkspaceChanged { name: String },
    ClipboardChanged,
    PortMessage { from: String, command: String, body: Option<String> },
    Timer { id: String },
    Shutdown,
}

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<DaemonEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.sender.subscribe()
    }

    #[allow(dead_code)]
    pub fn send(&self, event: DaemonEvent) {
        let _ = self.sender.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}
