pub struct Editor {
    pub buffer: String,
    pub cursor: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            buffer: String::new(),
            cursor: 0,
        }
    }

    pub fn append(&mut self, c: char) {
        let mut chars: Vec<char> = self.buffer.chars().collect();
        chars.insert(self.cursor, c);
        self.buffer = chars.iter().collect();
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if let Some(val) = self.cursor.checked_sub(1) {
            let mut chars: Vec<char> = self.buffer.chars().collect();
            chars.remove(self.cursor.saturating_sub(1));
            self.buffer = chars.iter().collect();
            self.cursor = val;
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = std::cmp::min(self.cursor + 1, self.buffer.len());
    }

    pub fn cursor_beginning(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    pub fn delete_word(&mut self) {
        for _ in 0..self
            .cursor.saturating_sub(self.previous_word_index())
        {
            self.backspace();
        }
    }

    pub fn next_word(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut found = false;
        for i in (self.cursor + 1)..self.buffer.chars().count() {
            if found {
                if let Some(' ') = chars.get(i) {
                    self.cursor = i;
                    return;
                }
            } else if let Some(c) = chars.get(i) {
                if c != &' ' {
                    found = true;
                }
            }
        }
        self.cursor = self.buffer.chars().count();
    }

    fn previous_word_index(&mut self) -> usize {
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut found = false;
        for i in (0..self.cursor.saturating_sub(1)).rev() {
            if found {
                if let Some(' ') = chars.get(i) {
                    return i + 1;
                }
            } else if let Some(c) = chars.get(i) {
                if c != &' ' {
                    found = true;
                }
            }
        }
        0
    }

    pub fn previous_word(&mut self) {
        self.cursor = self.previous_word_index();
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) -> String {
        self.cursor = 0;
        // Swaps contents of buffer with default String value and returns the previous contents.
        std::mem::take(&mut self.buffer)
    }
}
