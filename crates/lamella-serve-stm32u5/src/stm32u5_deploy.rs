//! A deploy's chunk, written into the deploy window of the STM32U5 C# interpreter firmware: which
//! pages the chunk erases before it is programmed, and what it programs. It touches no register, so
//! the firmware runs it over the part's own flash controller, and a host runs the same code over a
//! model of that controller.

use core::ops::Range;

/// The erase granule: one 8 KB page.
pub const PAGE: usize = 8 * 1024;
/// The programming granule: a 128-bit quad-word (RM0456 section 7.3.7, and the `csp/stm32u5a5`
/// flash block's write unit).
pub const QUAD_WORD: usize = 16;

/// The deploy window, as a chunk's write drives it. Offsets count from the window's first byte.
pub trait Window {
    /// The window's bytes `offset..offset + len`, as the flash reads now.
    fn read(&self, offset: usize, len: usize) -> &[u8];

    /// Erases the window's pages `pages`, and says whether every erase was clean and the pages now
    /// read erased.
    fn erase(&mut self, pages: Range<usize>) -> bool;

    /// Programs `bytes` from `offset`, which is on a quad-word boundary, with a short last
    /// quad-word padded with ones, and says whether the controller reported no error.
    fn program(&mut self, offset: usize, bytes: &[u8]) -> bool;
}

/// Writes a deploy's chunk at `offset`, which is on a quad-word boundary, and says whether the
/// chunk now reads back.
///
/// EVERY PAGE THAT STARTS INSIDE THIS CHUNK IS ERASED FIRST, WHATEVER IT READS. A quad-word
/// programmed with all ones reads exactly as an erased one does, and refuses the next program
/// (RM0456 Rev 7 sections 7.3.7 and 7.3.10), so a page that reads blank may still refuse a write:
/// every blank page of a part does, after a whole-flash write that programmed its ones. A page a
/// chunk begins partway into was erased by the chunk that began it. So a deploy erases each page
/// once, as it first reaches it, and one chunk's erase stays bounded by the chunk.
///
/// ONLY THE QUAD-WORDS THAT HOLD SOMETHING BUT ONES ARE PROGRAMMED. The erase left the others
/// reading as the chunk asks, and leaving them erased keeps them writable for whatever comes next.
pub fn write_chunk(window: &mut impl Window, offset: usize, chunk: &[u8]) -> bool {
    let end = offset + chunk.len();
    let pages = offset.div_ceil(PAGE)..end.div_ceil(PAGE);
    if !pages.is_empty() && !window.erase(pages) {
        return false;
    }
    let mut run: Option<usize> = None;
    for (index, quad) in chunk.chunks(QUAD_WORD).enumerate() {
        let at = index * QUAD_WORD;
        let ones = quad.iter().all(|&byte| byte == 0xFF);
        match run {
            None if !ones => run = Some(at),
            Some(start) if ones => {
                if !window.program(offset + start, &chunk[start..at]) {
                    return false;
                }
                run = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run
        && !window.program(offset + start, &chunk[start..])
    {
        return false;
    }
    window.read(offset, chunk.len()) == chunk
}
