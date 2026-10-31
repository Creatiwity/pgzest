use sqlparser::tokenizer::Location;

/// Conversion des positions sqlparser (ligne/colonne 1-basées, colonne en
/// caractères) en offsets octets dans la source.
pub(crate) struct LineIndex<'a> {
    source: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self {
            source,
            line_starts,
        }
    }

    /// Offset octet de `loc`. `None` pour une position hors source ou vide
    /// (sqlparser utilise la ligne 0 pour un span vide).
    pub(crate) fn offset(&self, loc: Location) -> Option<usize> {
        if loc.line == 0 || loc.column == 0 {
            return None;
        }
        let line_start = *self.line_starts.get((loc.line - 1) as usize)?;
        let line_end = self
            .line_starts
            .get(loc.line as usize)
            .copied()
            .unwrap_or(self.source.len());
        let line_text = self.source.get(line_start..line_end)?;
        // Avancer de (column - 1) caractères, pas d'octets.
        let byte_in_line = line_text
            .char_indices()
            .nth((loc.column - 1) as usize)
            .map(|(b, _)| b)
            .unwrap_or(line_text.len());
        Some(line_start + byte_in_line)
    }
}
