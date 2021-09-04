pub struct RealTimeBuffer<T> {
    head: u128, // next item to retrieve
    current_size: usize,
    max_size: usize,
    buffer: Vec<Option<T>>,
}

impl<T> RealTimeBuffer<T> {
    pub fn new(max_size: usize) -> RealTimeBuffer<T> {
        let mut buffer: Vec<Option<T>> = Vec::with_capacity(10);
        for i in 0..max_size {
            buffer.insert(i, None);
        }
        RealTimeBuffer {
            head: 0, current_size: 0, max_size, buffer
        }
    }
    fn head_index(&self) -> usize {
        (self.head % (self.max_size as u128)) as usize
    }
    pub fn len(&self) -> usize {
        self.current_size
    }
    pub fn set(&mut self, index: u128, data: T) {
        if index < self.head {
            return;
        }
        if self.buffer.swap_remove(self.head_index()).is_none() {
            self.current_size += 1;
        }
        self.buffer.insert(self.head_index(), Some(data));
        if (index - self.head) >= (self.max_size as u128) {
            self.head = index - (self.max_size as u128) + 1;
        }
    }
    pub fn next(&mut self) -> Option<T> {
        if self.current_size > 0 {
            let mut item: Option<T> = None;
            while item.is_none() {
                item = self.buffer.swap_remove(self.head_index());
                self.buffer.insert(self.head_index(), None);
                self.head += 1;
            }
            self.current_size -= 1;
            return item;
        } else {
            return None;
        }
    }
}