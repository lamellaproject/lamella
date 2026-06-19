//! Linux HID backend, implemented against the kernel hidraw interface.

#![allow(dead_code)]

use crate::{DeviceInfo, Error, Result};
use std::time::Duration;

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    Err(Error::Unsupported)
}

pub struct Device;

impl Device {
    pub fn open(_vendor_id: u16, _product_id: u16, _serial: Option<&str>) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn write_report(&mut self, _data: &[u8]) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub fn read_report(&mut self, _buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        Err(Error::Unsupported)
    }
}
