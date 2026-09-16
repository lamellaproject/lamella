//! USB Device Firmware Upgrade as an STM32 system bootloader speaks it: the class requests of the DFU
//! 1.1 specification and the DfuSe commands that ST's AN3156 adds to them, over any USB control pipe.

use std::fmt;

/// `DFU_DNLOAD`: a block of data to the device, or with no data the end of a download (DFU 1.1,
/// section 3).
pub const DFU_DNLOAD: u8 = 1;
/// `DFU_UPLOAD`: a block of data from the device.
pub const DFU_UPLOAD: u8 = 2;
/// `DFU_GETSTATUS`: the six-byte status reply.
pub const DFU_GETSTATUS: u8 = 3;
/// `DFU_CLRSTATUS`: leave the error state for idle.
pub const DFU_CLRSTATUS: u8 = 4;
/// `DFU_ABORT`: end a download or an upload and return to idle.
pub const DFU_ABORT: u8 = 6;

/// The DfuSe Set Address Pointer command byte (AN3156 Rev 18, 5.2).
const SET_ADDRESS_POINTER: u8 = 0x21;
/// The DfuSe Erase command byte, sent here only with a page address (AN3156 Rev 18, 5.3).
const ERASE: u8 = 0x41;
/// The block number every data block is sent as; see [`DfuSe::write`].
const DATA_BLOCK: u16 = 2;
/// The sizes a Write memory or Read memory block may have, in bytes (AN3156 Rev 18, 4.1 and 5.1).
const BLOCK_SIZES: std::ops::RangeInclusive<usize> = 2..=2048;

/// `bStatus`: the outcome of the most recent request (DFU 1.1, 6.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// `OK`: no error condition is present.
    Ok,
    /// `errTARGET`: the file is not targeted for use by this device.
    ErrTarget,
    /// `errFILE`: the file is for this device but fails a vendor-specific verification test.
    ErrFile,
    /// `errWRITE`: the device is unable to write memory.
    ErrWrite,
    /// `errERASE`: the memory erase function failed.
    ErrErase,
    /// `errCHECK_ERASED`: the memory erase check failed.
    ErrCheckErased,
    /// `errPROG`: the program memory function failed.
    ErrProg,
    /// `errVERIFY`: programmed memory failed verification.
    ErrVerify,
    /// `errADDRESS`: memory cannot be programmed at an address that is out of range.
    ErrAddress,
    /// `errNOTDONE`: a download of no data arrived before the device had all of the data.
    ErrNotDone,
    /// `errFIRMWARE`: the device's firmware is corrupt and it cannot return to run-time operation.
    ErrFirmware,
    /// `errVENDOR`: a vendor-specific error, which the status's string describes.
    ErrVendor,
    /// `errUSBR`: the device detected unexpected USB reset signaling.
    ErrUsbr,
    /// `errPOR`: the device detected an unexpected power-on reset.
    ErrPor,
    /// `errUNKNOWN`: something went wrong and the device does not know what.
    ErrUnknown,
    /// `errSTALLEDPKT`: the device stalled an unexpected request.
    ErrStalledPkt,
    /// A value the class specification does not list, kept as it arrived.
    Other(u8),
}

impl Status {
    /// The status `value` names, keeping a value the class specification does not list.
    #[must_use]
    pub fn from_byte(value: u8) -> Self {
        match value {
            0x00 => Status::Ok,
            0x01 => Status::ErrTarget,
            0x02 => Status::ErrFile,
            0x03 => Status::ErrWrite,
            0x04 => Status::ErrErase,
            0x05 => Status::ErrCheckErased,
            0x06 => Status::ErrProg,
            0x07 => Status::ErrVerify,
            0x08 => Status::ErrAddress,
            0x09 => Status::ErrNotDone,
            0x0A => Status::ErrFirmware,
            0x0B => Status::ErrVendor,
            0x0C => Status::ErrUsbr,
            0x0D => Status::ErrPor,
            0x0E => Status::ErrUnknown,
            0x0F => Status::ErrStalledPkt,
            other => Status::Other(other),
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Status::Ok => "OK",
            Status::ErrTarget => "errTARGET",
            Status::ErrFile => "errFILE",
            Status::ErrWrite => "errWRITE",
            Status::ErrErase => "errERASE",
            Status::ErrCheckErased => "errCHECK_ERASED",
            Status::ErrProg => "errPROG",
            Status::ErrVerify => "errVERIFY",
            Status::ErrAddress => "errADDRESS",
            Status::ErrNotDone => "errNOTDONE",
            Status::ErrFirmware => "errFIRMWARE",
            Status::ErrVendor => "errVENDOR",
            Status::ErrUsbr => "errUSBR",
            Status::ErrPor => "errPOR",
            Status::ErrUnknown => "errUNKNOWN",
            Status::ErrStalledPkt => "errSTALLEDPKT",
            Status::Other(value) => return write!(f, "status {value:#04x}"),
        };
        f.write_str(name)
    }
}

/// `bState`: the state the device enters as it sends a status reply (DFU 1.1, 6.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// `appIDLE`: the device is running its normal application.
    AppIdle,
    /// `appDETACH`: the application has received `DFU_DETACH` and waits for a USB reset.
    AppDetach,
    /// `dfuIDLE`: the device is in DFU mode and waiting for requests.
    DfuIdle,
    /// `dfuDNLOAD-SYNC`: a block has arrived and the device waits for `DFU_GETSTATUS`.
    DfuDnloadSync,
    /// `dfuDNBUSY`: the device is programming a block into nonvolatile memory.
    DfuDnbusy,
    /// `dfuDNLOAD-IDLE`: a download is in progress and the device expects `DFU_DNLOAD`.
    DfuDnloadIdle,
    /// `dfuMANIFEST-SYNC`: the final block has arrived, or manifestation has finished, and the device
    /// waits for `DFU_GETSTATUS`.
    DfuManifestSync,
    /// `dfuMANIFEST`: the device is in the manifestation phase.
    DfuManifest,
    /// `dfuMANIFEST-WAIT-RESET`: the device has programmed its memories and waits for a reset.
    DfuManifestWaitReset,
    /// `dfuUPLOAD-IDLE`: an upload is in progress and the device expects `DFU_UPLOAD`.
    DfuUploadIdle,
    /// `dfuERROR`: an error has occurred and the device waits for `DFU_CLRSTATUS`.
    DfuError,
    /// A value the class specification does not list, kept as it arrived.
    Other(u8),
}

impl State {
    /// The state `value` names, keeping a value the class specification does not list.
    #[must_use]
    pub fn from_byte(value: u8) -> Self {
        match value {
            0 => State::AppIdle,
            1 => State::AppDetach,
            2 => State::DfuIdle,
            3 => State::DfuDnloadSync,
            4 => State::DfuDnbusy,
            5 => State::DfuDnloadIdle,
            6 => State::DfuManifestSync,
            7 => State::DfuManifest,
            8 => State::DfuManifestWaitReset,
            9 => State::DfuUploadIdle,
            10 => State::DfuError,
            other => State::Other(other),
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            State::AppIdle => "appIDLE",
            State::AppDetach => "appDETACH",
            State::DfuIdle => "dfuIDLE",
            State::DfuDnloadSync => "dfuDNLOAD-SYNC",
            State::DfuDnbusy => "dfuDNBUSY",
            State::DfuDnloadIdle => "dfuDNLOAD-IDLE",
            State::DfuManifestSync => "dfuMANIFEST-SYNC",
            State::DfuManifest => "dfuMANIFEST",
            State::DfuManifestWaitReset => "dfuMANIFEST-WAIT-RESET",
            State::DfuUploadIdle => "dfuUPLOAD-IDLE",
            State::DfuError => "dfuERROR",
            State::Other(value) => return write!(f, "state {value}"),
        };
        f.write_str(name)
    }
}

/// The reply to `DFU_GETSTATUS` (DFU 1.1, 6.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusReport {
    /// `bStatus`: the outcome of the most recent request.
    pub status: Status,
    /// `bwPollTimeout`: the least time, in milliseconds, to wait before the next `DFU_GETSTATUS`.
    pub poll_timeout_ms: u32,
    /// `bState`: the state the device entered as it sent the reply.
    pub state: State,
}

impl StatusReport {
    /// Decodes a reply: `bStatus`, the three bytes of `bwPollTimeout` least significant first, `bState`
    /// and `iString`.
    ///
    /// # Errors
    /// A reply that is not six bytes long.
    pub fn parse(reply: &[u8]) -> Result<Self, DfuError> {
        let &[status, low, middle, high, state, _string] = reply else {
            return Err(DfuError::Malformed(format!(
                "a DFU_GETSTATUS reply is six bytes long, and this one is {}",
                reply.len()
            )));
        };
        Ok(Self {
            status: Status::from_byte(status),
            poll_timeout_ms: u32::from_le_bytes([low, middle, high, 0]),
            state: State::from_byte(state),
        })
    }
}

/// Why a DFU exchange stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DfuError {
    /// The control pipe failed, in its own words.
    Transport(String),
    /// The device reported a status or state that the step does not allow.
    Unexpected {
        /// What the host was doing.
        step: &'static str,
        /// What the step needed the device to report.
        expected: &'static str,
        /// What the device reported.
        report: StatusReport,
    },
    /// A reply was not the length the specification gives it.
    Malformed(String),
    /// The request cannot be sent as asked, and nothing was sent.
    Refused(String),
}

impl fmt::Display for DfuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DfuError::Transport(why) => write!(f, "the USB control pipe failed: {why}"),
            DfuError::Unexpected { step, expected, report } => write!(
                f,
                "{step}: the device reported {} in {}, where {expected} was expected",
                report.status, report.state
            ),
            DfuError::Malformed(why) | DfuError::Refused(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for DfuError {}

/// A USB control pipe bound to one DFU interface.
///
/// Every request is a class request to that interface, so an implementation fills `wIndex` with the
/// interface number, and the request type with `0x21` for [`class_out`](Self::class_out) and `0xA1`
/// for [`class_in`](Self::class_in) (DFU 1.1, section 3).
pub trait ControlPipe {
    /// Sends `request` with `wValue` set to `value` and `data` as the data stage, host to device.
    ///
    /// # Errors
    /// The transfer failing, a stall included.
    fn class_out(&mut self, request: u8, value: u16, data: &[u8]) -> Result<(), DfuError>;

    /// Sends `request` with `wValue` set to `value` and `wLength` set to the length of `buffer`, and
    /// answers how many bytes the device returned into `buffer`.
    ///
    /// # Errors
    /// The transfer failing, a stall included.
    fn class_in(&mut self, request: u8, value: u16, buffer: &mut [u8]) -> Result<usize, DfuError>;

    /// Waits at least `milliseconds`, as a status reply's `bwPollTimeout` asks.
    fn pause(&mut self, milliseconds: u32);
}

/// An STM32 system bootloader's DFU interface, driven by the sequences of AN3156.
///
/// Each operation leaves the device in `dfuIDLE` or `dfuDNLOAD-IDLE` with status OK -- the states a
/// following download is accepted in (AN3156 Rev 18, section 5) -- or fails with what the device
/// reported. On a device whose state is not known, call [`to_idle`](Self::to_idle) first.
pub struct DfuSe<P> {
    pipe: P,
    transfer_size: u16,
}

impl<P: ControlPipe> DfuSe<P> {
    /// Drives `pipe` with blocks of at most `transfer_size` bytes -- the `wTransferSize` of the
    /// interface's functional descriptor (DFU 1.1, 4.1.3).
    pub fn new(pipe: P, transfer_size: u16) -> Self {
        Self { pipe, transfer_size }
    }

    /// Gives the pipe back.
    pub fn into_pipe(self) -> P {
        self.pipe
    }

    /// Waits at least `milliseconds` between requests -- for a device fact the status reply does not
    /// carry, such as a bootloader version known to answer before an erase has finished.
    pub fn wait(&mut self, milliseconds: u32) {
        self.pipe.pause(milliseconds);
    }

    /// The device's status, from `DFU_GETSTATUS` (DFU 1.1, 6.1.2).
    ///
    /// # Errors
    /// The pipe failing, or a reply that is not six bytes long.
    pub fn status(&mut self) -> Result<StatusReport, DfuError> {
        let mut reply = [0u8; 6];
        let length = self.pipe.class_in(DFU_GETSTATUS, 0, &mut reply)?;
        StatusReport::parse(&reply[..length])
    }

    /// Brings the device to `dfuIDLE`, as AN3156 Rev 18 asks of a host before a download or an upload:
    /// an error is cleared with `DFU_CLRSTATUS` (DFU 1.1, 6.1.3), and a download or an upload in
    /// progress is ended with `DFU_ABORT` (DFU 1.1, 6.1.4).
    ///
    /// # Errors
    /// The pipe failing, a device in a state neither request leaves, or a device that does not report
    /// `dfuIDLE` with status OK afterwards.
    pub fn to_idle(&mut self) -> Result<(), DfuError> {
        const STEP: &str = "reaching dfuIDLE";
        let report = self.status()?;
        let request = match report.state {
            State::DfuIdle => return Ok(()),
            State::DfuError => DFU_CLRSTATUS,
            State::DfuDnloadIdle | State::DfuUploadIdle => DFU_ABORT,
            _ => {
                let expected = "dfuIDLE, dfuERROR, dfuDNLOAD-IDLE or dfuUPLOAD-IDLE";
                return Err(DfuError::Unexpected { step: STEP, expected, report });
            }
        };
        self.pipe.class_out(request, 0, &[])?;
        let report = self.status()?;
        if report.state == State::DfuIdle && report.status == Status::Ok {
            Ok(())
        } else {
            Err(DfuError::Unexpected { step: STEP, expected: "dfuIDLE with status OK", report })
        }
    }

    /// Sets the address pointer that later blocks are placed from (AN3156 Rev 18, 5.2).
    ///
    /// # Errors
    /// The pipe failing, or the device refusing -- `errTARGET` for an address it does not allow.
    pub fn set_address(&mut self, address: u32) -> Result<(), DfuError> {
        let [a0, a1, a2, a3] = address.to_le_bytes();
        self.download(0, &[SET_ADDRESS_POINTER, a0, a1, a2, a3], "setting the address pointer")
    }

    /// Erases the flash page, or sector, that starts at `address` (AN3156 Rev 18, 5.3).
    ///
    /// **There is no mass erase beside this, and that is deliberate:** AN2606 Rev 70, Table 136 lists
    /// DFU mass erase as not working in the STM32H74xxx/75xxx bootloader V13.3, so a writer erases the
    /// pages an image covers.
    ///
    /// # Errors
    /// The pipe failing, or the device refusing -- `errTARGET` for an address that is not a page
    /// start, `errVENDOR` while read protection is active.
    pub fn erase_page(&mut self, address: u32) -> Result<(), DfuError> {
        let [a0, a1, a2, a3] = address.to_le_bytes();
        self.download(0, &[ERASE, a0, a1, a2, a3], "erasing a page")
    }

    /// Writes `data` from `address`, one block at a time (AN3156 Rev 18, 5.1).
    ///
    /// **Every block is sent as block 2, at an address pointer set for that block.** The device
    /// places a block at `(wBlockNum - 2) * wTransferSize + address pointer`, where the transfer size
    /// is the length of the buffer it has just received -- so a short final block numbered past 2 would
    /// land short of where it belongs. A block numbered 2 lands at the pointer, whatever its length.
    ///
    /// # Errors
    /// A transfer size outside 2 to 2048 bytes, nothing to write, or data that would leave a final
    /// block of one byte -- each refused before anything is sent. Then the pipe failing, or the
    /// device refusing a block.
    pub fn write(&mut self, address: u32, data: &[u8]) -> Result<(), DfuError> {
        let size = self.block_size()?;
        if data.is_empty() {
            return Err(DfuError::Refused("there is nothing to write".to_owned()));
        }
        refuse_a_one_byte_final_block(data.len(), size)?;
        for (index, block) in data.chunks(size).enumerate() {
            self.set_address(block_address(address, index, size)?)?;
            self.download(DATA_BLOCK, block, "writing a block")?;
        }
        Ok(())
    }

    /// Reads `length` bytes from `address`, one block at a time (AN3156 Rev 18, 4.1).
    ///
    /// Each block sets the address pointer, ends that download with `DFU_ABORT`, uploads the block as
    /// block 2, and ends the upload with `DFU_ABORT`. An upload is accepted only in `dfuIDLE` and a
    /// download only in `dfuIDLE` or `dfuDNLOAD-IDLE` (DFU 1.1, A.2.3, A.2.6 and A.2.10), and a block
    /// numbered 2 is read from the pointer whatever its length, for the reason
    /// [`write`](Self::write) gives.
    ///
    /// # Errors
    /// A transfer size outside 2 to 2048 bytes or a final block of one byte, refused before anything
    /// is sent; then the pipe failing, the device refusing, or a block that comes back shorter than
    /// asked.
    pub fn read(&mut self, address: u32, length: usize) -> Result<Vec<u8>, DfuError> {
        let size = self.block_size()?;
        refuse_a_one_byte_final_block(length, size)?;
        let mut image = Vec::with_capacity(length);
        let mut index = 0;
        while image.len() < length {
            let wanted = size.min(length - image.len());
            let at = block_address(address, index, size)?;
            self.set_address(at)?;
            self.pipe.class_out(DFU_ABORT, 0, &[])?;
            let mut block = vec![0u8; wanted];
            let got = self.pipe.class_in(DFU_UPLOAD, DATA_BLOCK, &mut block)?;
            self.pipe.class_out(DFU_ABORT, 0, &[])?;
            if got != wanted {
                return Err(DfuError::Malformed(format!(
                    "the block at {at:#010x} came back {got} bytes long, and {wanted} were asked for"
                )));
            }
            image.extend_from_slice(&block);
            index += 1;
        }
        Ok(image)
    }

    /// Leaves DFU mode and starts the image at `address`: sets the address pointer to it, sends a
    /// download of no data, and asks for the status that runs the leave, which the device answers
    /// with `dfuMANIFEST` before it disconnects and jumps through the reset vector at `address + 4`
    /// (AN3156 Rev 18, 5.5).
    ///
    /// **THE STATUS REQUEST THAT RUNS THE LEAVE CAN FAIL AS THE DEVICE GOES.** AN3156 Rev 18, 5.5 says
    /// the leave "is effectively executed only when a DFU_GETSTATUS request is issued by the host",
    /// that the device then disconnects itself, and that "the device is unable to respond to host
    /// requests after a manifestation phase is completed"; DFU 1.1, A.2.8 has a device in
    /// `dfuMANIFEST` stall any class-specific request. So that request failing at the pipe is taken
    /// as the leave having run, and nothing is sent after it. A device that answers it with any state
    /// but `dfuMANIFEST` has not left.
    ///
    /// **A device that manifests has not necessarily started the image.** AN2606 Rev 70, Table 136
    /// lists, for the STM32H74xxx/75xxx bootloader V9.0, a jump that fails for an application whose
    /// stack pointer is not below the end of RAM less 16 bytes. Confirm a start by what the board
    /// does, and reset it when it does nothing.
    ///
    /// # Errors
    /// The pipe failing before the status request that runs the leave, or a device that answers that
    /// request with a state other than `dfuMANIFEST`.
    pub fn leave(&mut self, address: u32) -> Result<(), DfuError> {
        self.set_address(address)?;
        self.pipe.class_out(DFU_DNLOAD, 0, &[])?;
        let report = match self.status() {
            Ok(report) => report,
            Err(DfuError::Transport(_)) => return Ok(()),
            Err(other) => return Err(other),
        };
        if report.state == State::DfuManifest {
            Ok(())
        } else {
            Err(DfuError::Unexpected { step: "leaving DFU mode", expected: "dfuMANIFEST", report })
        }
    }

    /// A download and the two status requests that run it: the first must report `dfuDNBUSY`, and
    /// after the wait it asks for, the second must report `dfuDNLOAD-IDLE` with status OK (AN3156
    /// Rev 18, 5.1 and 5.2).
    fn download(&mut self, block: u16, data: &[u8], step: &'static str) -> Result<(), DfuError> {
        self.pipe.class_out(DFU_DNLOAD, block, data)?;
        let busy = self.status()?;
        if busy.state != State::DfuDnbusy {
            return Err(DfuError::Unexpected { step, expected: "dfuDNBUSY", report: busy });
        }
        self.pipe.pause(busy.poll_timeout_ms);
        let done = self.status()?;
        if done.state == State::DfuDnloadIdle && done.status == Status::Ok {
            Ok(())
        } else {
            let expected = "dfuDNLOAD-IDLE with status OK";
            Err(DfuError::Unexpected { step, expected, report: done })
        }
    }

    /// The block size the transfer size gives, refused where AN3156 Rev 18, 4.1 and 5.1 do not
    /// allow it.
    fn block_size(&self) -> Result<usize, DfuError> {
        let size = usize::from(self.transfer_size);
        if BLOCK_SIZES.contains(&size) {
            Ok(size)
        } else {
            Err(DfuError::Refused(format!(
                "a transfer size of {size} bytes is outside the 2 to 2048 bytes a block may carry \
                 (AN3156 Rev 18, 4.1 and 5.1)"
            )))
        }
    }
}

/// Refuses a length whose final block, at `size` bytes a block, would be one byte long -- a block is 2
/// to 2048 bytes (AN3156 Rev 18, 4.1 and 5.1).
fn refuse_a_one_byte_final_block(length: usize, size: usize) -> Result<(), DfuError> {
    if length % size == 1 {
        return Err(DfuError::Refused(format!(
            "{length} bytes leave a final block of one byte, and a block is 2 to 2048 bytes \
             (AN3156 Rev 18, 4.1 and 5.1); pad the image to the part's program alignment"
        )));
    }
    Ok(())
}

/// Where the block at `index` of an operation from `address` starts, at `size` bytes a block.
fn block_address(address: u32, index: usize, size: usize) -> Result<u32, DfuError> {
    index
        .checked_mul(size)
        .and_then(|offset| u32::try_from(offset).ok())
        .and_then(|offset| address.checked_add(offset))
        .ok_or_else(|| {
            DfuError::Refused(format!(
                "block {index} from {address:#010x} lies past the end of the 32-bit address space"
            ))
        })
}

/// The class, subclass and protocol of a DFU interface in DFU mode (DFU 1.1, Table 4.4).
pub const DFU_MODE: lamella_usbbulk::InterfaceClass =
    lamella_usbbulk::InterfaceClass { class: 0xFE, subclass: 0x01, protocol: 0x02 };

/// The DFU functional descriptor's `bDescriptorType` (DFU 1.1, Table 4.2).
const DFU_FUNCTIONAL: u8 = 0x21;

/// How long one control transfer to a bootloader may take before it is cancelled: the five seconds
/// USB 2.0, 9.2.6.4 gives a request whose data stage goes to the device, the longest limit that
/// section sets, which 9.2.6.5 applies to class requests too. The waits a device asks for between
/// requests are not part of it; those are [`ControlPipe::pause`]s, taken between transfers.
const TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What a DFU interface's functional descriptor states (DFU 1.1, 4.1.3 and Table 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionalDescriptor {
    /// `bitCanDnload`: the device accepts a download.
    pub can_download: bool,
    /// `bitCanUpload`: the device answers an upload.
    pub can_upload: bool,
    /// `bitManifestationTolerant`: the device still answers over USB after manifestation.
    pub manifestation_tolerant: bool,
    /// `bitWillDetach`: the device detaches and reattaches itself when it receives `DFU_DETACH`.
    pub will_detach: bool,
    /// `wDetachTimeOut`, in milliseconds.
    pub detach_timeout_ms: u16,
    /// `wTransferSize`: the most bytes the device takes in one control-write transaction.
    pub transfer_size: u16,
    /// `bcdDFUVersion`, as the binary-coded decimal the device sent.
    pub dfu_version: u16,
}

impl FunctionalDescriptor {
    /// Finds and decodes the functional descriptor among a configuration's descriptors, where it
    /// follows the interface descriptor it extends (USB 2.0, 9.4.3).
    ///
    /// # Errors
    /// A configuration that carries no functional descriptor, or one shorter than the nine bytes
    /// Table 4.2 gives it.
    pub fn find(configuration: &[u8]) -> Result<Self, DfuError> {
        let descriptor = lamella_usbbulk::descriptors(configuration)
            .find(|descriptor| descriptor[1] == DFU_FUNCTIONAL)
            .ok_or_else(|| {
                DfuError::Malformed(
                    "the configuration carries no DFU functional descriptor (DFU 1.1, 4.1.3)"
                        .to_owned(),
                )
            })?;
        let &[_, _, attributes, detach_low, detach_high, size_low, size_high, version_low, version_high, ..] =
            descriptor
        else {
            return Err(DfuError::Malformed(format!(
                "a DFU functional descriptor is nine bytes long, and this one is {}",
                descriptor.len()
            )));
        };
        Ok(Self {
            can_download: attributes & 0x01 != 0,
            can_upload: attributes & 0x02 != 0,
            manifestation_tolerant: attributes & 0x04 != 0,
            will_detach: attributes & 0x08 != 0,
            detach_timeout_ms: u16::from_le_bytes([detach_low, detach_high]),
            transfer_size: u16::from_le_bytes([size_low, size_high]),
            dfu_version: u16::from_le_bytes([version_low, version_high]),
        })
    }
}

/// The class request `request` to interface `interface`, as every DFU request is sent (DFU 1.1,
/// section 3).
fn class_request(request: u8, value: u16, interface: u8) -> lamella_usbbulk::ControlRequest {
    lamella_usbbulk::ControlRequest {
        kind: lamella_usbbulk::RequestKind::Class,
        recipient: lamella_usbbulk::Recipient::Interface,
        request,
        value,
        index: u16::from(interface),
    }
}

/// A [`ControlPipe`] over a claimed USB interface.
pub struct UsbPipe {
    interface: lamella_usbbulk::ControlInterface,
}

impl UsbPipe {
    /// Drives the DFU interface that `interface` claimed.
    pub fn new(interface: lamella_usbbulk::ControlInterface) -> Self {
        Self { interface }
    }

    /// Reads the device's first configuration whole, by GET_DESCRIPTOR: its first nine bytes for
    /// `wTotalLength` (USB 2.0, Table 9-10), then every byte that states (USB 2.0, 9.4.3).
    ///
    /// # Errors
    /// The pipe failing, or a device that returns less than its configuration states.
    pub fn configuration(&mut self) -> Result<Vec<u8>, DfuError> {
        let mut head = [0u8; 9];
        let got = self.get_configuration(&mut head)?;
        if got < 4 {
            return Err(DfuError::Malformed(format!(
                "a configuration descriptor is nine bytes long, and the device returned {got}"
            )));
        }
        let total = usize::from(u16::from_le_bytes([head[2], head[3]]));
        let mut whole = vec![0u8; total];
        let got = self.get_configuration(&mut whole)?;
        if got != total {
            return Err(DfuError::Malformed(format!(
                "the configuration states {total} bytes, and the device returned {got}"
            )));
        }
        Ok(whole)
    }

    /// One GET_DESCRIPTOR for the first configuration: request 6, with descriptor type 2 in the high
    /// byte of `wValue` and index 0 in its low byte (USB 2.0, Tables 9-4 and 9-5, and 9.4.3).
    fn get_configuration(&mut self, buffer: &mut [u8]) -> Result<usize, DfuError> {
        const GET_DESCRIPTOR: u8 = 6;
        const CONFIGURATION: u16 = 2;
        let request = lamella_usbbulk::ControlRequest {
            kind: lamella_usbbulk::RequestKind::Standard,
            recipient: lamella_usbbulk::Recipient::Device,
            request: GET_DESCRIPTOR,
            value: CONFIGURATION << 8,
            index: 0,
        };
        self.interface
            .control_in(request, buffer, TRANSFER_TIMEOUT)
            .map_err(|why| DfuError::Transport(why.to_string()))
    }
}

impl ControlPipe for UsbPipe {
    fn class_out(&mut self, request: u8, value: u16, data: &[u8]) -> Result<(), DfuError> {
        let request = class_request(request, value, self.interface.interface_number());
        self.interface
            .control_out(request, data, TRANSFER_TIMEOUT)
            .map_err(|why| DfuError::Transport(why.to_string()))
    }

    fn class_in(&mut self, request: u8, value: u16, buffer: &mut [u8]) -> Result<usize, DfuError> {
        let request = class_request(request, value, self.interface.interface_number());
        self.interface
            .control_in(request, buffer, TRANSFER_TIMEOUT)
            .map_err(|why| DfuError::Transport(why.to_string()))
    }

    fn pause(&mut self, milliseconds: u32) {
        std::thread::sleep(std::time::Duration::from_millis(u64::from(milliseconds)));
    }
}

/// The DFU interface a write goes to, among the attached `devices` under `vendor_id`: the one whose
/// serial is `serial` -- whole, and without regard to case -- when one is named, and otherwise the
/// only one attached.
///
/// # Errors
/// A named serial that no device reports, no device at all, or several with none named. Each
/// refusal lists every device it considered, so the reader can name one.
pub fn choose_device<'a>(
    devices: &'a [lamella_usbbulk::InterfaceInfo],
    vendor_id: u16,
    serial: Option<&str>,
) -> Result<&'a lamella_usbbulk::InterfaceInfo, String> {
    let candidates: Vec<&lamella_usbbulk::InterfaceInfo> =
        devices.iter().filter(|device| device.vendor_id == vendor_id).collect();
    let listing = candidates.iter().map(|device| describe_device(device)).collect::<Vec<_>>().join("\n");
    if let Some(serial) = serial.map(str::trim) {
        let named: Vec<&lamella_usbbulk::InterfaceInfo> = candidates
            .iter()
            .copied()
            .filter(|device| {
                device.serial_number.as_deref().is_some_and(|reported| reported.eq_ignore_ascii_case(serial))
            })
            .collect();
        return match named.as_slice() {
            [one] => Ok(one),
            [] if candidates.is_empty() => Err(format!(
                "no DFU bootloader under vendor {vendor_id:04x} is attached, so none reports serial {serial}."
            )),
            [] => Err(format!(
                "no attached DFU bootloader reports serial {serial}. Attached under vendor {vendor_id:04x}:\n{listing}\n\n\
                 Name one with --device <serial>."
            )),
            several => Err(format!(
                "{} attached DFU bootloaders report serial {serial}, so it names none of them:\n{listing}",
                several.len()
            )),
        };
    }
    match candidates.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no DFU bootloader under vendor {vendor_id:04x} is attached.")),
        several => Err(format!(
            "{} DFU bootloaders are attached under vendor {vendor_id:04x} and none was named:\n{listing}\n\n\
             Name one with --device <serial>.",
            several.len()
        )),
    }
}

/// One attached DFU interface, as a refusal lists it.
fn describe_device(device: &lamella_usbbulk::InterfaceInfo) -> String {
    format!(
        "  {:04x}:{:04x}  serial {}  {}",
        device.vendor_id,
        device.product_id,
        device.serial_number.as_deref().unwrap_or("(none reported)"),
        device.product.as_deref().unwrap_or("")
    )
}

/// What AN2606 states about one STM32 series' system bootloader that a DFU write depends on.
#[derive(Debug)]
pub struct SystemBootloader {
    /// The series, as AN2606 names it.
    pub series: &'static str,
    /// Where the bootloader keeps its one-byte ID: the last byte but one of the series' system memory
    /// (AN2606 Rev 70, 4.2 and Table 3).
    pub id_address: u32,
    /// Every bootloader version AN2606 lists for the series, as the ID each reports and its name.
    pub versions: &'static [(u8, &'static str)],
    /// A version whose erase in the second bank answers before that erase has finished, and how many
    /// milliseconds to wait after each such erase before the next command.
    pub early_bank2_erase: Option<(u8, u32)>,
    /// What an ID read from this bootloader settles, for the write's report.
    pub settles: &'static str,
}

/// The STM32H74xxx/75xxx system bootloader (AN2606 Rev 70, chapter 60).
///
/// Its ID is at `0x1FF1E7FE` (Table 3): the last byte but one of the 122 KB of system memory from
/// `0x1FF00000` that Table 135 gives the bootloader. The versions are Table 136's.
///
/// **V9.1 LISTS "ERASE ON BANK2 NOT WORKING AS EXPECTED"**: the bootloader answers while the erase is
/// still running, a command that reaches the flash before it ends can hang the part, and AN2606's
/// workaround is to wait the datasheet's worst-case erase time after the erase. The wait here is 4 s,
/// the longest maximum sector erase time DS12930 Rev 1, Table 64 lists for the STM32H747, at a
/// parallelism of 8. That table is stated for a single-bank configuration and names no parallelism for
/// the bootloader, so the longest figure it gives is taken. V9.2 lists the defect as fixed.
pub const STM32H74X_75X_BOOTLOADER: SystemBootloader = SystemBootloader {
    series: "STM32H74xxx/75xxx",
    id_address: 0x1FF1_E7FE,
    versions: &[(0xD2, "V13.2"), (0xD3, "V13.3"), (0x90, "V9.0"), (0x91, "V9.1"), (0x92, "V9.2")],
    early_bank2_erase: Some((0x91, 4_000)),
    settles: "the ID byte of an STM32H74xxx/75xxx system bootloader: a bootloader version, which \
              names that series and no board",
};

impl crate::StFamily {
    /// The system bootloader this family's parts carry, where this build knows one.
    ///
    /// **ONLY WHERE EVERY PART THE PLAN COVERS IS OF THE BOOTLOADER's SERIES.** The H7 plan covers the
    /// H745, H747, H755 and H757, which are AN2606's STM32H74xxx/75xxx; an H723 or an H7A3 carries
    /// another bootloader and is no part of that plan.
    pub fn system_bootloader(self) -> Option<&'static SystemBootloader> {
        match self {
            crate::StFamily::H7 => Some(&STM32H74X_75X_BOOTLOADER),
            crate::StFamily::L0
            | crate::StFamily::C0
            | crate::StFamily::L4
            | crate::StFamily::U5
            | crate::StFamily::F7 => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod scripted;
#[cfg(test)]
mod tests;
