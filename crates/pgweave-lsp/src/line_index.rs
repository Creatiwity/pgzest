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

impl LineIndex {
    /// The inverse: (line, character) → byte offset.
    ///
    /// LSP columns count UTF-16 units; this approximates with characters,
    /// which coincide outside the astral plane (emojis). Enough to locate a
    /// cursor in SQL, and clamped to the line so it never overflows.
    pub fn offset_at(&self, line: usize, character: usize) -> usize {
        let Some(&start) = self.line_starts.get(line) else {
            return 0;
        };
        let end = self
            .line_starts
            .get(line + 1)
            .map(|&e| e as usize)
            .unwrap_or(usize::MAX);
        (start as usize + character).min(end.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_at_is_the_inverse_of_offset_to_position() {
        let text = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let idx = LineIndex::new(text);
        for offset in [0u32, 5, 10, 15, 20, 25] {
            let (line, col) = idx.offset_to_position(offset);
            assert_eq!(
                idx.offset_at(line as usize, col as usize),
                offset as usize,
                "round trip on offset {offset}"
            );
        }
    }

    #[test]
    fn offset_at_is_clamped_to_the_line() {
        let text = "ab\ncd\n";
        let idx = LineIndex::new(text);
        // Column well past the end of line 0: clamped before the \n.
        assert!(idx.offset_at(0, 99) < 3);
    }

    #[test]
    fn a_missing_line_returns_zero() {
        let idx = LineIndex::new("ab");
        assert_eq!(idx.offset_at(42, 0), 0);
    }
}
