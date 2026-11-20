//! Where a deploy-capable firmware keeps a LOADED artifact while a plain run or a debug session
//! borrows it: one heap allocation, freed when the transfer arena lets go of it.
//!
//! The runner can only give the loader the `'static` bytes it borrows by keeping an image alive,
//! and a firmware that holds its transfer arena across frames, as a chunked transfer needs,
//! completes every LOAD it is sent. Leaking each image would keep one per run until the heap ran
//! out. This keeps one at a time.
//!
//! A firmware binary includes this file with `#[path]`, as it includes `startup.rs`, and passes a
//! `OneLoadedImage` to `lamella_runner::serve_one_deploy_with_residence` beside its arena.

extern crate alloc;

/// One loaded image at a time: the image placed last, kept as the `Vec` it arrived in.
///
/// It hands each image out as a `'static` slice and frees it later, so whoever calls `admit` and
/// `release` decides whether a slice outlives its bytes. The runner's serve functions call them in
/// an order in which none can, and nothing else should call them, which is why making one is
/// `unsafe` (see [`OneLoadedImage::new`]).
pub struct OneLoadedImage {
    /// The image placed last: its pointer, length and capacity, as the `Vec` it arrived in.
    held: Option<(*mut u8, usize, usize)>,
}

impl OneLoadedImage {
    /// A residence holding no image.
    ///
    /// # Safety
    ///
    /// `admit`, `release` and this value's drop each free the image placed before them, so each
    /// must come when nothing will read a slice an earlier `admit` returned. The serve functions of
    /// `lamella_runner` call `admit` and `release` only so: `release` once the transfer arena has let
    /// go of the image, with nothing executing, and `admit` only after that. So hand this residence
    /// only to those functions, beside the one `ArtifactLoad` they place its images in; never call
    /// `admit` or `release` yourself; and drop it only after that arena, or never, as a serve loop
    /// that runs for the board's life does.
    pub unsafe fn new() -> Self {
        Self { held: None }
    }
}

impl lamella_runner::ImageResidence for OneLoadedImage {
    fn admit(&mut self, image: alloc::vec::Vec<u8>) -> Option<&'static [u8]> {
        lamella_runner::ImageResidence::release(self);
        let mut image = core::mem::ManuallyDrop::new(image);
        let held = (image.as_mut_ptr(), image.len(), image.capacity());
        self.held = Some(held);
        // SAFETY: the allocation is live until `release` or the drop rebuilds the Vec it came from
        // and drops it, and `new`'s contract puts both after the last read of this slice: the
        // runner releases only once its arena has let go of these bytes with nothing executing --
        // an execution refuses every transfer while it exists.
        Some(unsafe { core::slice::from_raw_parts(held.0, held.1) })
    }

    fn release(&mut self) {
        if let Some((pointer, len, capacity)) = self.held.take() {
            // SAFETY: the three parts are exactly those of the Vec `admit` took apart, and it was
            // never dropped; `new`'s contract puts this call after the last read of the slice it
            // handed out.
            drop(unsafe { alloc::vec::Vec::from_raw_parts(pointer, len, capacity) });
        }
    }
}

impl Drop for OneLoadedImage {
    fn drop(&mut self) {
        lamella_runner::ImageResidence::release(self);
    }
}
