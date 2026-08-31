/// Maps byte offsets to (line, character) pairs for LSP position reporting.
/// Lines and characters are 0-indexed (LSP convention).
pub struct LineIndex {
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0u32];
        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                starts.push((i + 1) as u32);
            }
        }
        Self {
            line_starts: starts,
        }
    }

    /// Convert a byte offset to (line, character). Returns (0,0) if out of range.
    pub fn offset_to_position(&self, offset: u32) -> (u32, u32) {
        let offset = offset as usize;
        match self.line_starts.binary_search(&(offset as u32)) {
            Ok(line) => (line as u32, 0),
            Err(line) => {
                let line = line.saturating_sub(1);
                let col = offset - self.line_starts[line] as usize;
                (line as u32, col as u32)
            }
        }
    }
}
