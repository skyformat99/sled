use std::ptr;

use super::*;

/// A pending log reservation which can be aborted or completed.
/// NB the holder should quickly call `complete` or `abort` as
/// taking too long to decide will cause the underlying IO
/// buffer to become blocked.
pub struct Reservation<'a> {
    pub(super) iobufs: &'a IoBufs,
    pub idx: usize,
    pub data: Vec<u8>,
    pub destination: &'a mut [u8],
    pub flushed: bool,
    pub lsn: Lsn,
    pub lid: LogID,
}

impl<'a> Drop for Reservation<'a> {
    fn drop(&mut self) {
        // We auto-abort if the user never uses a reservation.
        let should_flush = !self.data.is_empty() && !self.flushed;
        if should_flush {
            self.flush(false);
        }
    }
}

impl<'a> Reservation<'a> {
    /// Cancel the reservation, placing a failed flush on disk, returning
    /// the (cancelled) log sequence number and file offset.
    pub fn abort(mut self) -> (Lsn, LogID) {
        self.flush(false)
    }

    /// Complete the reservation, placing the buffer on disk. returns
    /// the log sequence number of the write, and the file offset.
    pub fn complete(mut self) -> (Lsn, LogID) {
        self.flush(true)
    }

    /// Get the log file offset for reading this buffer in the future.
    pub fn lid(&self) -> LogID {
        self.lid
    }

    /// Get the log sequence number for this update.
    pub fn lsn(&self) -> Lsn {
        self.lsn
    }

    fn flush(&mut self, valid: bool) -> (Lsn, LogID) {
        if self.flushed {
            panic!("flushing already-flushed reservation!");
        }

        self.flushed = true;

        if valid {
            self.destination.copy_from_slice(&*self.data);
        } else {
            // zero the bytes, as aborted reservations skip writing
            unsafe {
                ptr::write_bytes(
                    self.destination.as_ptr() as *mut u8,
                    0,
                    self.data.len(),
                );
            }
        }

        self.iobufs.exit_reservation(self.idx);

        (self.lsn(), self.lid())
    }
}
