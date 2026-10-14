//! A board at the other end of the wire, in this process, for testing a host that sends a program
//! to firmware: what the host sends is served by the deploy serve a board's firmware runs, over a
//! flash region in memory.
//!
//! Test support. It is built for this crate's own tests, and for another crate's when that crate
//! turns on the `loopback-board` feature, which a crate does in its `[dev-dependencies]`.

use lamella_wire::{Transport, TransportError};

/// A RAM flash region as a board's flash sink presents one: an image is read where it was
/// written, and an erase clears the region's first 4 KB sector, which is what invalidates a
/// stored image.
pub struct RamFlash {
    /// What the region holds.
    pub region: &'static [u8],
    /// How many times the board was told to erase it.
    pub erases: usize,
}

impl RamFlash {
    /// A 64 KB region holding `image` at its start, and erased flash after it.
    #[must_use]
    pub fn holding(image: &[u8]) -> Self {
        let mut region = vec![0xFF; 64 * 1024];
        region[..image.len()].copy_from_slice(image);
        Self { region: Box::leak(region.into_boxed_slice()), erases: 0 }
    }

    /// An erased region of `len` bytes, for an image larger than [`RamFlash::holding`] has room
    /// for: one that carries a library besides the corlib.
    #[must_use]
    pub fn erased(len: usize) -> Self {
        Self { region: Box::leak(vec![0xFF; len].into_boxed_slice()), erases: 0 }
    }

    fn rewrite(&mut self, edit: impl FnOnce(&mut [u8])) {
        let mut region = self.region.to_vec();
        edit(&mut region);
        self.region = Box::leak(region.into_boxed_slice());
    }
}

impl lamella_runner::FlashSink for RamFlash {
    fn image_slice(&self) -> &'static [u8] {
        self.region
    }
    fn erase(&mut self) {
        self.erases += 1;
        self.rewrite(|region| region[..4096].fill(0xFF));
    }
    fn program(&mut self, image: &[u8]) -> bool {
        let fits = image.len() <= self.region.len();
        if fits {
            self.rewrite(|region| {
                region.fill(0xFF);
                region[..image.len()].copy_from_slice(image);
            });
        }
        fits
    }
    fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
        let fits = total <= self.region.len() && offset + chunk.len() <= self.region.len();
        if fits {
            self.rewrite(|region| {
                if offset == 0 {
                    region.fill(0xFF);
                }
                region[offset..offset + chunk.len()].copy_from_slice(chunk);
            });
        }
        fits
    }
}

/// Make every board this module serves, in this process, answer a HELLO as `product_model`, as a
/// board's firmware reports its own model.
///
/// It is one setting for the whole process, as a model is one per board, so a test that needs a
/// particular model runs in a process of its own or with tests that need the same one.
pub fn answer_as(product_model: u16) {
    lamella_runner::set_board_identity(product_model, 0, 0);
}

/// A board at the other end of the wire, in this process: every frame the host sends is served
/// by the deploy serve a board's firmware runs, over `flash`, and its replies wait to be read.
pub struct LoopbackBoard {
    replies: lamella_wire::MemTransport,
    carrier: lamella_wire::MemTransport,
    /// The board's flash.
    pub flash: RamFlash,
    /// ONE arena for the board's life, as a firmware holds it.
    load: lamella_runner::ArtifactLoad,
    /// The serve asked for the reset that starts the stored program. Once its last replies are
    /// read, the carrier is gone, as a board's own USB is when it resets.
    pub reset: bool,
    /// How many deploy chunks the host has sent.
    pub chunks: usize,
}

impl LoopbackBoard {
    /// A board whose flash is `flash`, serving nothing yet.
    #[must_use]
    pub fn on(flash: RamFlash) -> Self {
        Self {
            replies: lamella_wire::MemTransport::new(),
            carrier: lamella_wire::MemTransport::new(),
            flash,
            load: lamella_runner::ArtifactLoad::new(),
            reset: false,
            chunks: 0,
        }
    }
}

impl Transport for LoopbackBoard {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        if msg_type == crate::deploy::DEPLOY_IMAGE {
            self.chunks += 1;
        }
        let mut host = lamella_wire::MemTransport::new();
        Transport::send(&mut host, msg_type, seq, payload)?;
        self.carrier.feed(&host.take_sent());
        loop {
            match lamella_runner::serve_one_deploy(&mut self.carrier, &mut self.flash, &mut self.load)? {
                lamella_runner::Served::Nothing => break,
                lamella_runner::Served::RunRequested => self.reset = true,
                _ => {}
            }
        }
        self.replies.feed(&self.carrier.take_sent());
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
        match Transport::poll(&mut self.replies)? {
            None if self.reset => Err(TransportError::Closed),
            polled => Ok(polled),
        }
    }
}

/// Serve the host at the other end of `carrier` as a board's firmware serves it, over `flash`,
/// until the host goes away or asks for the reset that starts the stored program: for a board on
/// a carrier a test hands a host, such as a socket.
///
/// # Errors
/// The carrier's error once the host has gone, which is how this ends unless a reset was asked for.
pub fn serve(carrier: &mut impl Transport, flash: &mut RamFlash) -> Result<(), TransportError> {
    let mut load = lamella_runner::ArtifactLoad::new();
    loop {
        match lamella_runner::serve_one_deploy(carrier, flash, &mut load)? {
            lamella_runner::Served::RunRequested => return Ok(()),
            lamella_runner::Served::Nothing => std::thread::sleep(std::time::Duration::from_millis(1)),
            _ => {}
        }
    }
}
