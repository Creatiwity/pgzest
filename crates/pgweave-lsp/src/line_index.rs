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
    /// L'inverse : (ligne, caractère) → offset octet.
    ///
    /// Les colonnes LSP comptent des unités UTF-16 ; on approxime par des
    /// caractères, ce qui coïncide hors plan astral (émojis). Suffisant pour
    /// situer un curseur dans du SQL, et borné à la ligne pour ne jamais
    /// déborder.
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
    fn offset_at_est_l_inverse_de_offset_to_position() {
        let text = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let idx = LineIndex::new(text);
        for offset in [0u32, 5, 10, 15, 20, 25] {
            let (line, col) = idx.offset_to_position(offset);
            assert_eq!(
                idx.offset_at(line as usize, col as usize),
                offset as usize,
                "aller-retour sur l'offset {offset}"
            );
        }
    }

    #[test]
    fn offset_at_est_borne_a_la_ligne() {
        let text = "ab\ncd\n";
        let idx = LineIndex::new(text);
        // Colonne bien au-delà de la fin de la ligne 0 : borné avant le \n.
        assert!(idx.offset_at(0, 99) < 3);
    }

    #[test]
    fn une_ligne_inexistante_renvoie_zero() {
        let idx = LineIndex::new("ab");
        assert_eq!(idx.offset_at(42, 0), 0);
    }
}
