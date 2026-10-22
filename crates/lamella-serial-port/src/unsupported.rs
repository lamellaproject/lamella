//! An operating system with no serial port implementation here: every open is refused.

use std::time::Duration;

use crate::{Buffer, Error, Settings};

/// Whether opening `given` opens the port the listing names `listed`: here no port opens, and no
/// port is listed, so only the same name is the same port.
pub(crate) fn opens_listed(given: &str, listed: &str) -> bool {
    given == listed
}

/// A port that cannot exist: the only constructor always refuses.
#[derive(Debug)]
pub(crate) enum Port {}

impl Port {
    pub(crate) fn open(_name: &str, _settings: &Settings) -> Result<Port, Error> {
        Err(Error::UnsupportedHost)
    }

    pub(crate) fn read(&mut self, _buffer: &mut [u8], _timeout: Option<Duration>) -> Result<usize, Error> {
        match *self {}
    }

    pub(crate) fn write(&mut self, _bytes: &[u8], _timeout: Option<Duration>) -> Result<usize, Error> {
        match *self {}
    }

    pub(crate) fn set_dtr(&mut self, _on: bool) -> Result<(), Error> {
        match *self {}
    }

    pub(crate) fn set_rts(&mut self, _on: bool) -> Result<(), Error> {
        match *self {}
    }

    pub(crate) fn discard(&mut self, _which: Buffer) -> Result<(), Error> {
        match *self {}
    }

    pub(crate) fn bytes_to_read(&mut self) -> Result<usize, Error> {
        match *self {}
    }

    pub(crate) fn bytes_to_write(&mut self) -> Result<usize, Error> {
        match *self {}
    }

    pub(crate) fn drain(&mut self) -> Result<(), Error> {
        match *self {}
    }

    pub(crate) fn close_after_sending(&mut self) -> Result<(), Error> {
        match *self {}
    }
}
