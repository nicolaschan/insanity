pub struct RealTimeBuffer<T> {
    head: u128, // next item to retrieve
    current_size: usize,
    max_size: usize,
    buffer: Vec<Option<T>>,
}

impl<T> RealTimeBuffer<T> {
    pub fn new(max_size: usize) -> RealTimeBuffer<T> {
        let mut buffer: Vec<Option<T>> = Vec::with_capacity(max_size);
        for i in 0..max_size {
            buffer.insert(i, None);
        }
        RealTimeBuffer {
            head: 0, current_size: 0, max_size, buffer
        }
    }

    pub fn len(&self) -> usize {
        self.current_size
    }
    pub fn set(&mut self, index: u128, data: T) {
        if index < self.head {
            return; // you got data you already skipped in the past
        }

        let real_index = (index % (self.max_size as u128)) as usize;
        if self.buffer[real_index].is_none() {
            self.current_size += 1;
        }
        self.buffer[real_index] = Some(data);

        // you receive data too far in the future (like a full cycle around the buffer)
        if (index - self.head) >= (self.max_size as u128) {
            self.head = index - (self.max_size as u128) + 1;
        }
    }
    pub fn next(&mut self) -> Option<T> {
        let head_index = (self.head % self.max_size as u128) as usize;

        let mut current = None;
        return if self.current_size > 0 {
            while current.is_none() {
                current = self.buffer[head_index].take();
                self.head += 1;
            }
            self.current_size -= 1;
            current
        } else {
            None
        }
    }
}