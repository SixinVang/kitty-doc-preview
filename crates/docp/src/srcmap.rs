//! Mapping between editor source lines, parsed blocks and layout rows.
//!
//! `render_md::doc::Block` carries the byte range of its top-level source, so a
//! line can be resolved to a block (and back) without depending on block
//! indices, which change on every reload.

use render_md::doc::Block;

/// Byte offsets of the start of every line in a document.
///
/// Lines are 0-based here; the editor-facing helpers convert to 1-based.
#[derive(Clone, Debug, Default)]
pub struct LineMap {
    starts: Vec<usize>,
    len: usize,
}

impl LineMap {
    pub fn new(src: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        Self {
            starts,
            len: src.len(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Byte offset where a (0-based) line starts. Out-of-range lines clamp.
    pub fn line_start(&self, line: usize) -> usize {
        self.starts[line.min(self.starts.len() - 1)]
    }

    /// 0-based line containing a byte offset.
    pub fn byte_to_line(&self, byte: usize) -> usize {
        let byte = byte.min(self.len);
        self.starts
            .partition_point(|&s| s <= byte)
            .saturating_sub(1)
    }

    /// 1-based line containing a byte offset.
    pub fn byte_to_line1(&self, byte: usize) -> usize {
        self.byte_to_line(byte) + 1
    }

    /// Byte offset of a 1-based line, clamped to the document.
    pub fn line_to_byte(&self, line1: usize) -> usize {
        self.line_start(line1.saturating_sub(1))
    }
}

/// The block whose source contains the given 1-based line.
///
/// Falls back to the nearest preceding block, then to the first block, so a
/// line in a blank gap (or before the first block) still resolves.
pub fn line_block(blocks: &[Block], lines: &LineMap, line1: usize) -> usize {
    if blocks.is_empty() {
        return 0;
    }
    let byte = lines.line_to_byte(line1);
    // `src.start` is non-decreasing in block order.
    blocks
        .partition_point(|b| b.src.start <= byte)
        .saturating_sub(1)
}

/// The first source line (1-based) of a block.
pub fn block_line(blocks: &[Block], lines: &LineMap, block: usize) -> usize {
    match blocks.get(block) {
        Some(b) => lines.byte_to_line1(b.src.start),
        None => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> (Vec<Block>, LineMap) {
        (render_md::doc::parse(src).blocks, LineMap::new(src))
    }

    #[test]
    fn lines_and_bytes_round_trip() {
        let src = "a\nbb\n\nccc";
        let m = LineMap::new(src);
        assert_eq!(m.line_count(), 4);
        assert_eq!(m.line_start(0), 0);
        assert_eq!(m.line_start(1), 2);
        assert_eq!(m.line_start(3), 6);
        assert_eq!(m.byte_to_line(0), 0);
        assert_eq!(m.byte_to_line(1), 0); // the newline itself ends line 0
        assert_eq!(m.byte_to_line(2), 1);
        assert_eq!(m.byte_to_line(100), 3); // clamps past the end
        assert_eq!(m.byte_to_line1(6), 4);
        assert_eq!(m.line_to_byte(99), 6);
        assert_eq!(m.line_to_byte(0), 0);
    }

    #[test]
    fn empty_document_is_one_line() {
        let m = LineMap::new("");
        assert_eq!(m.line_count(), 1);
        assert_eq!(m.byte_to_line1(0), 1);
        assert_eq!(m.line_to_byte(1), 0);
    }

    #[test]
    fn lines_map_to_blocks_and_back() {
        let src = "# Title\n\nfirst para\nwraps here\n\n- a\n- b\n\nlast\n";
        let (blocks, m) = doc(src);
        assert_eq!(blocks.len(), 4);
        assert_eq!(block_line(&blocks, &m, 0), 1);
        assert_eq!(block_line(&blocks, &m, 1), 3);
        assert_eq!(block_line(&blocks, &m, 2), 6);
        assert_eq!(block_line(&blocks, &m, 3), 9);

        for (i, b) in blocks.iter().enumerate() {
            let first = block_line(&blocks, &m, i);
            let last = m.byte_to_line(b.src.end.saturating_sub(1)) + 1;
            for line in first..=last {
                assert_eq!(line_block(&blocks, &m, line), i, "line {line}");
            }
        }
        // The blank separator before a block belongs to the preceding block.
        assert_eq!(line_block(&blocks, &m, 5), 1);
        // Out-of-range lines fall back to the last block.
        assert_eq!(line_block(&blocks, &m, 10_000), 3);
    }

    #[test]
    fn empty_blocks_are_safe() {
        let blocks: Vec<Block> = Vec::new();
        let m = LineMap::new("x\n");
        assert_eq!(line_block(&blocks, &m, 1), 0);
        assert_eq!(block_line(&blocks, &m, 0), 1);
    }
}
