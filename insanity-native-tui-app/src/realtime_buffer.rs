pub struct RealTimeBuffer<T> {
    head: u128, // next item to retrieve
    current_size: usize,
    max_size: usize,
    buffer: Vec<Option<(u128, T)>>,
    prev: u128,
    seen_any: bool,
}

impl<T> RealTimeBuffer<T> {
    pub fn new(max_size: usize) -> RealTimeBuffer<T> {
        assert!(max_size > 0, "RealTimeBuffer capacity must be > 0");
        let mut buffer: Vec<Option<(u128, T)>> = Vec::with_capacity(max_size);
        for i in 0..max_size {
            buffer.insert(i, None);
        }
        RealTimeBuffer {
            head: 0,
            current_size: 0,
            prev: 0,
            max_size,
            buffer,
            seen_any: false,
        }
    }

    pub fn len(&self) -> usize {
        self.current_size
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Next sequence number expected for playback (read cursor).
    pub fn head(&self) -> u128 {
        self.head
    }
    /// Max sequence number seen via `set` (write cursor).
    pub fn prev(&self) -> u128 {
        self.prev
    }

    /// Drop all buffered entries with seq < `new_head`. Caller sets head after.
    fn evict_stale(&mut self, new_head: u128) {
        for slot in self.buffer.iter_mut() {
            if let Some((seq, _)) = slot
                && *seq < new_head
            {
                *slot = None;
                self.current_size = self.current_size.saturating_sub(1);
            }
        }
    }

    /// Reset for a fresh stream (e.g. reconnect with restarted seq numbers).
    pub fn clear(&mut self) {
        for slot in self.buffer.iter_mut() {
            *slot = None;
        }
        self.current_size = 0;
        self.head = 0;
        self.prev = 0;
        self.seen_any = false;
    }

    pub fn set(&mut self, index: u128, data: T) {
        if index < self.head {
            return; // you got data you already skipped in the past
        }
        if self.seen_any {
            if index > self.prev {
                self.prev = index;
            }
        } else {
            self.prev = index;
            self.seen_any = true;
            // First-ever chunk defines the read cursor so a stream starting
            // at nonzero seq doesn't force a gap walk from 0.
            if self.current_size == 0 {
                self.head = index;
            }
        }

        let real_index = (index % (self.max_size as u128)) as usize;
        match self.buffer[real_index].take() {
            None => {
                self.buffer[real_index] = Some((index, data));
                self.current_size += 1;
            }
            Some((old_seq, _old_data)) if old_seq == index => {
                // Duplicate redelivery: replace, size unchanged.
                self.buffer[real_index] = Some((index, data));
            }
            Some((old_seq, _)) => {
                let _ = old_seq;
                self.buffer[real_index] = Some((index, data));
            }
        }

        // you receive data too far in the future (like a full cycle around the buffer)
        if (index - self.head) >= (self.max_size as u128) {
            let new_head = index - (self.max_size as u128) + 1;
            self.evict_stale(new_head);
            self.head = new_head;
        }
    }
    pub fn next_item(&mut self) -> Option<T> {
        // Preserve timing: exactly one seq slot per call. A missing head slot
        // yields concealment (None) and advances head by one, even when future
        // data is already buffered. Starvation past `prev` yields None without
        // advancing (wait for new data).
        if self.head > self.prev && self.current_size == 0 {
            return None;
        }
        let head_index = (self.head % self.max_size as u128) as usize;
        match self.buffer[head_index].take() {
            Some((seq, data)) if seq == self.head => {
                self.head += 1;
                self.current_size = self.current_size.saturating_sub(1);
                Some(data)
            }
            Some((seq, _)) if seq < self.head => {
                self.current_size = self.current_size.saturating_sub(1);
                if self.head > self.prev {
                    return None;
                }
                self.head += 1;
                None
            }
            Some((seq, data)) => {
                if seq > self.prev {
                    self.current_size = self.current_size.saturating_sub(1);
                    let _ = data;
                } else {
                    self.buffer[head_index] = Some((seq, data));
                }
                self.head += 1;
                None
            }
            None => {
                if self.head > self.prev {
                    return None;
                }
                self.head += 1;
                None
            }
        }
    }
}
