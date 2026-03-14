use std::collections::VecDeque;

/// Ring buffer for token history sparklines
pub struct TokenHistory {
    buffer: VecDeque<u64>,
    capacity: usize,
}

impl TokenHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, value: u64) {
        if self.buffer.len() >= self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(value);
    }

    pub fn as_vec(&self) -> Vec<u64> {
        self.buffer.iter().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }
}
