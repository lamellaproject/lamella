use super::*;
use alloc::collections::VecDeque;

/// A board's RAM holding a console ring, with the image's writer.
struct Ram {
    at: u32,
    bytes: Vec<u8>,
    /// Text the board writes the moment a read of the buffer (not the header) completes.
    during_reads: VecDeque<Vec<u8>>,
    /// A header to hand back, once, in place of the next header read: a read whose words were taken
    /// across an end's two stores.
    torn: Option<[u8; 16]>,
}

impl Ram {
    /// A ring of `size` bytes at `at`, armed as a start arms it.
    fn armed(at: u32, size: u32) -> Ram {
        let mut ram = Ram { at, bytes: vec![0; 16 + size as usize], during_reads: VecDeque::new(), torn: None };
        ram.set_word(1, size);
        ram.set_word(0, RUNNING);
        ram
    }

    fn word(&self, index: usize) -> u32 {
        let at = index * 4;
        u32::from_le_bytes([self.bytes[at], self.bytes[at + 1], self.bytes[at + 2], self.bytes[at + 3]])
    }

    fn set_word(&mut self, index: usize, value: u32) {
        self.bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// The image's tee: each byte lands at the count modulo the size, then the count moves.
    fn emit(&mut self, text: &[u8]) {
        let size = self.word(1);
        for &byte in text {
            let written = self.word(2);
            self.bytes[16 + (written % size) as usize] = byte;
            self.set_word(2, written.wrapping_add(1));
        }
    }

    /// The image's end: the code, whole, then the end magic in place of `RLOG`.
    fn end(&mut self, magic: u32, code: i32) {
        self.set_word(3, code as u32);
        self.set_word(0, magic);
    }

    /// The ring as its image's ELF gives it: here, and every byte of it.
    fn object(&self) -> DataObject {
        DataObject { address: self.at, size: self.bytes.len() as u32 }
    }
}

impl TargetMemory for Ram {
    fn read(&mut self, address: u32, into: &mut [u8]) -> Result<(), String> {
        let start = (address - self.at) as usize;
        if start == 0
            && into.len() == 16
            && let Some(torn) = self.torn.take()
        {
            into.copy_from_slice(&torn);
            return Ok(());
        }
        into.copy_from_slice(&self.bytes[start..start + into.len()]);
        if start >= 16
            && let Some(text) = self.during_reads.pop_front()
        {
            self.emit(&text);
        }
        Ok(())
    }
}

/// A reader of `ram`'s ring, patient for three looks.
fn reader(ram: &Ram) -> RingReader {
    RingReader::new(ram.object(), END_MAGICS, 3)
}

/// **THE RING IS READ AS THE IMAGE WRITES IT**: what is new since the last look, across the buffer's
/// end, and the program's end only after its last bytes.
#[test]
fn the_ring_delivers_what_is_new_across_its_end_and_then_the_programs_end() {
    let mut ram = Ram::armed(0x2000_8000, 8);
    let mut console = reader(&ram);
    assert_eq!(console.poll(&mut ram), Ok(Vec::new()), "nothing written yet");
    ram.emit(b"hello");
    assert_eq!(console.poll(&mut ram), Ok(vec![Seen::Out(b"hello".to_vec())]));
    ram.emit(b" world");
    assert_eq!(console.poll(&mut ram), Ok(vec![Seen::Out(b" world".to_vec())]), "across the buffer's end");
    ram.emit(b"!");
    ram.end(END_MAGICS.returned, 3);
    assert_eq!(
        console.poll(&mut ram),
        Ok(vec![Seen::Out(b"!".to_vec()), Seen::Ended(ProgramEnd::Returned(3))]),
        "the last byte, then the end"
    );
}

/// **THE CODE COMES BACK WHOLE**, past a byte and past sixteen bits, negative too, for either way a
/// program ends.
#[test]
fn the_code_beside_an_end_magic_is_the_whole_word() {
    let cases = [
        (END_MAGICS.returned, 70_000, ProgramEnd::Returned(70_000)),
        (END_MAGICS.returned, i32::MAX, ProgramEnd::Returned(i32::MAX)),
        (END_MAGICS.returned, -1, ProgramEnd::Returned(-1)),
        (END_MAGICS.aborted, 134, ProgramEnd::Aborted(134)),
        (END_MAGICS.aborted, i32::MIN, ProgramEnd::Aborted(i32::MIN)),
    ];
    for (magic, code, end) in cases {
        let mut ram = Ram::armed(0x2000_8000, 32);
        ram.end(magic, code);
        assert_eq!(reader(&ram).poll(&mut ram), Ok(vec![Seen::Ended(end)]), "{magic:#x} beside {code}");
    }
}

/// **A CODE BESIDE `RLOG` IS NO END**: it is the instant between an end's two stores, and the
/// program has not ended until the magic says so.
#[test]
fn a_code_beside_the_running_magic_is_not_an_end() {
    let mut ram = Ram::armed(0x2000_8000, 32);
    ram.emit(b"still going");
    ram.set_word(3, 42);
    assert_eq!(reader(&ram).poll(&mut ram), Ok(vec![Seen::Out(b"still going".to_vec())]));
}

/// **AN END IS READ AGAIN BEFORE IT IS TAKEN**: a header whose words were read across the end's two
/// stores carries an end magic beside an old code and count, and the second read is final.
#[test]
fn an_end_is_read_again_so_its_code_and_count_are_final() {
    let mut ram = Ram::armed(0x2000_8000, 32);
    let mut torn = [0u8; 16];
    for (index, word) in [END_MAGICS.returned, 32, 2, 0].into_iter().enumerate() {
        torn[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    ram.emit(b"done\n");
    ram.end(END_MAGICS.returned, 7);
    ram.torn = Some(torn);
    assert_eq!(
        reader(&ram).poll(&mut ram),
        Ok(vec![Seen::Out(b"done\n".to_vec()), Seen::Ended(ProgramEnd::Returned(7))])
    );
}

/// **THE END MAGICS ARE THE ONES THE READER WAS GIVEN**: under another spelling the board's `RRET`
/// is no end, and the given one is.
#[test]
fn the_end_magics_are_injected() {
    let other = EndMagics { returned: 0x1111_1111, aborted: 0x2222_2222 };
    let mut ram = Ram::armed(0x2000_8000, 32);
    ram.end(END_MAGICS.returned, 5);
    let mut console = RingReader::new(ram.object(), other, 3);
    assert_eq!(console.poll(&mut ram), Ok(Vec::new()), "not a ring this reader knows yet");
    ram.end(other.aborted, 9);
    assert_eq!(console.poll(&mut ram), Ok(vec![Seen::Ended(ProgramEnd::Aborted(9))]));
}

/// **WHAT THE BOARD OVERWROTE BEFORE IT COULD BE READ IS COUNTED, NOT SHOWN** -- whether it was gone
/// before the look or went while the look was reading it.
#[test]
fn bytes_the_board_overwrote_are_counted_rather_than_shown() {
    let mut ram = Ram::armed(0x2000_8000, 8);
    let mut console = reader(&ram);
    ram.emit(b"0123456789AB");
    assert_eq!(console.poll(&mut ram), Ok(vec![Seen::Lost(4), Seen::Out(b"456789AB".to_vec())]));

    ram.emit(b"CDEFGH");
    ram.during_reads.push_back(b"IJKLMN".to_vec());
    assert_eq!(console.poll(&mut ram), Ok(vec![Seen::Lost(4), Seen::Out(b"GH".to_vec())]));
    assert_eq!(console.poll(&mut ram), Ok(vec![Seen::Out(b"IJKLMN".to_vec())]), "and they are read next");
}

/// **A COUNT THAT GOES BACKWARDS IS A NEW START**, followed from its beginning.
#[test]
fn a_ring_set_up_again_is_a_new_start() {
    let mut ram = Ram::armed(0x2000_8000, 16);
    let mut console = reader(&ram);
    ram.emit(b"first boot");
    assert_eq!(console.poll(&mut ram), Ok(vec![Seen::Out(b"first boot".to_vec())]));
    ram.set_word(2, 0);
    ram.emit(b"again");
    assert_eq!(console.poll(&mut ram), Ok(vec![Seen::Restarted, Seen::Out(b"again".to_vec())]));
}

/// **A RING NOT SET UP YET IS WAITED FOR, BRIEFLY**, and then reported where it was looked for; a
/// header that claims a bigger buffer than the image gives the ring is no ring's.
#[test]
fn a_ring_that_never_appears_is_reported_where_it_was_looked_for() {
    let mut ram = Ram::armed(0x2000_8000, 8);
    ram.set_word(0, 0);
    let mut console = reader(&ram);
    for _ in 0..3 {
        assert_eq!(console.poll(&mut ram), Ok(Vec::new()));
    }
    let error = console.poll(&mut ram).expect_err("no ring");
    assert!(error.contains("0x20008000"), "{error}");

    let mut ram = Ram::armed(0x2000_8000, 8);
    ram.set_word(1, 9);
    assert!(reader(&ram).poll(&mut ram).expect_err("no ring").contains("not the ring the image gives it"));
    ram.set_word(1, 0x0100_0000);
    let no_size = DataObject { address: 0x2000_8000, size: 0 };
    let mut console = RingReader::new(no_size, END_MAGICS, 3);
    assert!(console.poll(&mut ram).expect_err("no ring").contains("not the ring the image gives it"));
}

/// An aborted program arrives as the run's end like any other.
#[test]
fn an_aborted_program_arrives_through_its_end_magic() {
    let mut ram = Ram::armed(0x2000_8000, 32);
    let mut console = reader(&ram);
    ram.emit(b"Unhandled exception.\n");
    ram.end(END_MAGICS.aborted, 134);
    assert_eq!(
        console.poll(&mut ram),
        Ok(vec![Seen::Out(b"Unhandled exception.\n".to_vec()), Seen::Ended(ProgramEnd::Aborted(134))])
    );
}

/// A debuggable executable naming `objects`, as the class-library build writes one.
fn elf_naming(objects: &[(&'static str, u32, u32)]) -> Vec<u8> {
    let named: Vec<lamella_elf::NamedObject<'_>> = objects
        .iter()
        .map(|&(name, address, size)| lamella_elf::NamedObject { name, section: name, address, size })
        .collect();
    lamella_elf::write_debuggable_executable_with_objects(
        lamella_elf::Machine::Arm,
        &[0x00, 0xBF],
        0,
        0x1000_0000,
        true,
        &[],
        &named,
    )
}

/// **THE RING IS FOUND BY EITHER IMAGE'S SYMBOL**, at the address and size the ELF gives it.
#[test]
fn the_ring_is_found_by_either_images_symbol() {
    let ring = DataObject { address: 0x2000_8000, size: 16 + 16384 };
    assert_eq!(ring_in(&elf_naming(&[(LAMELLA_RING, ring.address, ring.size)])), Ok((LAMELLA_RING, ring)));
    assert_eq!(ring_in(&elf_naming(&[(RHU_RING, ring.address, ring.size)])), Ok((RHU_RING, ring)));
    let no_size = DataObject { address: 0x2000_8000, size: 0 };
    assert_eq!(ring_in(&elf_naming(&[(LAMELLA_RING, 0x2000_8000, 0)])), Ok((LAMELLA_RING, no_size)));
}

/// **ONE RING OR NONE**: an ELF naming neither symbol, or both, gives no ring to follow, and so does
/// one too small to hold the header.
#[test]
fn one_ring_or_none() {
    assert_eq!(ring_in(&elf_naming(&[("something_else", 0x2000_0000, 64)])), Err(NoRing::Neither));
    assert_eq!(ring_in(b"not an ELF"), Err(NoRing::Neither));
    let both = elf_naming(&[(LAMELLA_RING, 0x2000_8000, 64), (RHU_RING, 0x2003_0000, 64)]);
    assert_eq!(ring_in(&both), Err(NoRing::Both));
    assert_eq!(
        ring_in(&elf_naming(&[(LAMELLA_RING, 0x2000_8000, 16)])),
        Err(NoRing::TooSmall { symbol: LAMELLA_RING, size: 16 })
    );
}
