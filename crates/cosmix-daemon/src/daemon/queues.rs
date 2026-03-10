use std::collections::{HashMap, VecDeque};

/// Session-scoped named queues (ARexx PUSH/PULL equivalent).
#[derive(Debug, Default)]
pub struct QueueStore {
    queues: HashMap<String, VecDeque<serde_json::Value>>,
}

impl QueueStore {
    pub fn new() -> Self {
        Self { queues: HashMap::new() }
    }

    pub fn push(&mut self, name: &str, value: serde_json::Value) {
        self.queues.entry(name.to_string()).or_default().push_back(value);
    }

    pub fn pop(&mut self, name: &str) -> Option<serde_json::Value> {
        self.queues.get_mut(name)?.pop_front()
    }

    pub fn size(&self, name: &str) -> usize {
        self.queues.get(name).map_or(0, |q| q.len())
    }

    pub fn list(&self) -> Vec<(String, usize)> {
        self.queues.iter().map(|(k, v)| (k.clone(), v.len())).collect()
    }

    pub fn clear(&mut self, name: &str) {
        self.queues.remove(name);
    }
}
