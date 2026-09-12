pub struct JitterBuffer<T> {
    head: u128, // next item to retrieve
    current_size: usize,
    max_size: usize,
    buffer: Vec<Option<(u128, T)>>,
    prev: u128,
    seen_any: bool,
}

impl<T> JitterBuffer<T> {
    pub fn new(max_size: usize) -> JitterBuffer<T> {
        assert!(max_size > 0, "JitterBuffer capacity must be > 0");
        let buffer: Vec<Option<(u128, T)>> = (0..max_size).map(|_| None).collect();
        JitterBuffer {
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

    pub fn reset(&mut self) {
        for slot in self.buffer.iter_mut() {
            *slot = None;
        }
        self.head = 0;
        self.prev = 0;
        self.current_size = 0;
        self.seen_any = false;
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
    fn evict_stale(&mut self, new_head: u128) -> usize {
        let mut dropped = 0;
        for slot in self.buffer.iter_mut() {
            if let Some((seq, _)) = slot
                && *seq < new_head
            {
                *slot = None;
                self.current_size = self.current_size.saturating_sub(1);
                dropped += 1;
            }
        }
        dropped
    }

    pub fn set(&mut self, index: u128, data: T) -> usize {
        if index < self.head {
            return 0; // you got data you already skipped in the past
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

        // you receive data too far in the future (like a full cycle around the buffer)
        let mut dropped = 0;
        if (index - self.head) >= (self.max_size as u128) {
            let new_head = index - (self.max_size as u128) + 1;
            dropped += self.evict_stale(new_head);
            self.head = new_head;
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
            Some(_) => {
                dropped += 1;
                self.buffer[real_index] = Some((index, data));
            }
        }
        dropped
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

#[cfg(test)]
mod tests {
    use super::JitterBuffer;

    #[test]
    fn occupancy_bounded_by_capacity() {
        let mut buffer = JitterBuffer::new(3);
        buffer.set(0, 0u32);
        buffer.set(1, 1u32);
        buffer.set(2, 2u32);
        assert_eq!(buffer.len(), 3);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn gap_walk_advances_one_slot_per_call() {
        let mut buffer = JitterBuffer::new(10);
        buffer.set(0, 0u32);
        buffer.set(2, 2u32);
        assert_eq!(buffer.next_item(), Some(0));
        assert_eq!(buffer.next_item(), None);
        assert_eq!(buffer.next_item(), Some(2));
    }

    #[test]
    fn starvation_past_prev_waits_without_advancing() {
        let mut buffer = JitterBuffer::new(10);
        buffer.set(5, 5u32);
        assert_eq!(buffer.next_item(), Some(5));
        assert_eq!(buffer.head(), 6);
        assert_eq!(buffer.next_item(), None);
        assert_eq!(buffer.next_item(), None);
        assert_eq!(buffer.head(), 6);
    }

    #[test]
    fn late_data_dropped_after_playout() {
        let mut buffer = JitterBuffer::new(10);
        buffer.set(0, 0u32);
        assert_eq!(buffer.next_item(), Some(0));
        buffer.set(0, 9u32);
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.next_item(), None);
    }

    #[test]
    fn duplicate_replaces_without_growth() {
        let mut buffer = JitterBuffer::new(10);
        buffer.set(3, 3u32);
        buffer.set(3, 4u32);
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.next_item(), Some(4));
    }

    #[test]
    fn far_future_evicts_and_slides_head() {
        let mut buffer = JitterBuffer::new(3);
        buffer.set(0, 0u32);
        buffer.set(1, 1u32);
        buffer.set(10, 10u32);
        assert_eq!(buffer.head(), 8);
        assert_eq!(buffer.next_item(), None);
        assert_eq!(buffer.next_item(), None);
        assert_eq!(buffer.next_item(), Some(10));
    }

    #[test]
    fn virgin_nonzero_seq_anchors_head() {
        let mut buffer = JitterBuffer::new(10);
        buffer.set(7, 7u32);
        assert_eq!(buffer.head(), 7);
        assert_eq!(buffer.next_item(), Some(7));
    }

    #[test]
    fn overflow_reports_dropped_unplayed() {
        let mut buffer = JitterBuffer::new(3);
        buffer.set(0, 0u32);
        buffer.set(1, 1u32);
        buffer.set(2, 2u32);
        assert_eq!(buffer.set(5, 5u32), 3);
        assert_eq!(buffer.head(), 3);
        assert_eq!(buffer.next_item(), None);
        assert_eq!(buffer.next_item(), None);
        assert_eq!(buffer.next_item(), Some(5));
    }

    #[test]
    fn reset_clears_and_reanchors() {
        let mut buffer = JitterBuffer::new(10);
        buffer.set(0, 0u32);
        assert_eq!(buffer.next_item(), Some(0));
        buffer.reset();
        assert!(buffer.is_empty());
        assert_eq!(buffer.head(), 0);
        buffer.set(0, 9u32);
        assert_eq!(buffer.next_item(), Some(9));
    }
}
