//! Correspondance offsets du SQL généré → offsets du `.pgsql` source.
//!
//! Sans cela un diagnostic portant sur le corps d'une fonction pointerait une
//! position dans le SQL généré, invisible pour qui édite le `.pgsql`.
//!
//! L'expansion recopie les corps **verbatim**, donc chaque segment recopié a un
//! décalage constant : la correspondance est exacte, pas approchée. Le
//! boilerplate généré (`CREATE FUNCTION … AS $$`) n'a pas d'antécédent dans la
//! source et pointe donc sur l'en-tête de la macro.

/// Un fragment du SQL généré et son antécédent dans la source.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    /// Offset du début du fragment dans le SQL généré.
    generated: usize,
    /// Offset du début du fragment dans le `.pgsql`.
    source: usize,
    /// Longueur du fragment, en octets.
    len: usize,
}

/// Correspondance généré → source.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    segments: Vec<Segment>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Déclare que `len` octets à `generated` proviennent de `source`.
    pub fn add(&mut self, generated: usize, source: usize, len: usize) {
        if len == 0 {
            return;
        }
        // Fusionner avec le segment précédent s'il est contigu de part et d'autre.
        if let Some(last) = self.segments.last_mut()
            && last.generated + last.len == generated
            && last.source + last.len == source
        {
            last.len += len;
            return;
        }
        self.segments.push(Segment {
            generated,
            source,
            len,
        });
    }

    /// Antécédent de `offset` dans la source, si ce fragment vient de la source.
    ///
    /// `None` pour du boilerplate généré, qui n'a pas d'antécédent — l'appelant
    /// se rabat alors sur [`Self::nearest_source`].
    pub fn to_source(&self, offset: usize) -> Option<usize> {
        let seg = self.segment_at(offset)?;
        Some(seg.source + (offset - seg.generated))
    }

    /// Antécédent de `offset`, ou à défaut le début du segment recopié le plus
    /// proche en amont. Toujours une position exploitable pour un diagnostic.
    pub fn nearest_source(&self, offset: usize) -> Option<usize> {
        if let Some(exact) = self.to_source(offset) {
            return Some(exact);
        }
        self.segments
            .iter()
            .rev()
            .find(|s| s.generated <= offset)
            .map(|s| s.source + s.len.saturating_sub(1))
            .or_else(|| self.segments.first().map(|s| s.source))
    }

    fn segment_at(&self, offset: usize) -> Option<&Segment> {
        // Segments ajoutés dans l'ordre croissant de `generated` : recherche
        // dichotomique sur le début.
        let idx = self
            .segments
            .partition_point(|s| s.generated <= offset)
            .checked_sub(1)?;
        let seg = &self.segments[idx];
        (offset < seg.generated + seg.len).then_some(seg)
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_segment_simple_se_traduit() {
        let mut m = SourceMap::new();
        m.add(100, 10, 5); // généré[100..105] ← source[10..15]
        assert_eq!(m.to_source(100), Some(10));
        assert_eq!(m.to_source(104), Some(14));
        assert_eq!(m.to_source(105), None, "hors du segment");
        assert_eq!(m.to_source(99), None);
    }

    #[test]
    fn les_segments_contigus_fusionnent() {
        let mut m = SourceMap::new();
        m.add(0, 50, 10);
        m.add(10, 60, 10); // contigu des deux côtés
        assert_eq!(m.segments.len(), 1, "doit fusionner");
        assert_eq!(m.to_source(15), Some(65));
    }

    #[test]
    fn les_segments_non_contigus_restent_distincts() {
        let mut m = SourceMap::new();
        m.add(0, 50, 10);
        m.add(10, 200, 10); // même généré, source ailleurs
        assert_eq!(m.segments.len(), 2);
        assert_eq!(m.to_source(5), Some(55));
        assert_eq!(m.to_source(15), Some(205));
    }

    #[test]
    fn plusieurs_segments_se_traduisent_chacun() {
        let mut m = SourceMap::new();
        m.add(0, 0, 10);
        m.add(30, 100, 10);
        assert_eq!(m.to_source(5), Some(5));
        assert_eq!(m.to_source(35), Some(105));
        assert_eq!(m.to_source(20), None, "boilerplate entre les deux");
    }

    #[test]
    fn nearest_source_rabat_le_boilerplate() {
        let mut m = SourceMap::new();
        m.add(0, 0, 10);
        m.add(30, 100, 10);
        // offset 20 = boilerplate : se rabat sur la fin du segment précédent
        assert_eq!(m.nearest_source(20), Some(9));
        assert_eq!(m.nearest_source(5), Some(5), "exact quand disponible");
    }

    #[test]
    fn une_map_vide_ne_traduit_rien() {
        let m = SourceMap::new();
        assert_eq!(m.to_source(0), None);
        assert_eq!(m.nearest_source(0), None);
        assert!(m.is_empty());
    }

    #[test]
    fn un_segment_de_longueur_nulle_est_ignore() {
        let mut m = SourceMap::new();
        m.add(0, 0, 0);
        assert!(m.is_empty());
    }
}
