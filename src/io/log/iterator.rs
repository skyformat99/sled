use std::io::{Error, ErrorKind};

use super::*;

pub struct Iter<'a> {
    pub(super) config: &'a Config,
    pub(super) segment_iter: Box<Iterator<Item = (Lsn, LogID)>>,
    pub(super) segment_base: Option<LogID>,
    pub(super) segment_len: usize,
    pub(super) use_compression: bool,
    pub(super) max_lsn: Lsn,
    pub(super) cur_lsn: Lsn,
    pub(super) trailer: Option<Lsn>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (Lsn, LogID, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        // If segment is None, get next on segment_iter, panic
        // if we can't read something we expect to be able to,
        // return None if there are no more remaining segments.
        loop {
            let at_end = !valid_entry_offset(self.cur_lsn, self.segment_len);
            if self.trailer.is_none() && at_end {
                // We've read to the end of a torn
                // segment and should stop now.
                return None;
            } else if self.segment_base.is_none() || at_end {
                if let Some((next_lsn, next_lid)) = self.segment_iter.next() {
                    assert!(
                        next_lsn + (self.segment_len as Lsn) >= self.cur_lsn,
                        "caller is responsible for providing segments \
                            that contain the initial cur_lsn value or higher"
                    );
                    if let Err(e) = self.read_segment(next_lsn, next_lid) {
                        debug!(
                            "hit snap while reading segments in \
                            iterator: {:?}",
                            e
                        );
                        return None;
                    }
                } else {
                    return None;
                }
            }

            if self.cur_lsn > self.max_lsn {
                // all done
                return None;
            }

            let lid = self.segment_base.unwrap() +
                (self.cur_lsn % self.segment_len as LogID);

            if self.max_lsn <= lid {
                // we've hit the end of the log.
                trace!(
                    "in Iter::next self.max_lsn {} <= lid {}",
                    self.max_lsn,
                    lid
                );
                return None;
            }

            let cached_f = self.config.cached_file();
            let mut f = cached_f.borrow_mut();
            match f.read_message(lid, self.segment_len, self.use_compression) {
                Ok(LogRead::Flush(lsn, buf, on_disk_len)) => {
                    trace!("read flush in Iter::next");
                    self.cur_lsn += (MSG_HEADER_LEN + on_disk_len) as LogID;
                    return Some((lsn, lid, buf));
                }
                Ok(LogRead::Zeroed(on_disk_len)) => {
                    trace!("read zeroed in Iter::next");
                    self.cur_lsn += on_disk_len as LogID;
                }
                _ => {
                    trace!("read failed in Iter::next");
                    if self.trailer.is_none() {
                        // This segment was torn, nothing left to read.
                        return None;
                    }
                    self.segment_base.take();
                    self.trailer.take();
                    continue;
                }
            }
        }
    }
}

impl<'a> Iter<'a> {
    /// read a segment of log messages. Only call after
    /// pausing segment rewriting on the segment accountant!
    fn read_segment(&mut self, lsn: Lsn, offset: LogID) -> std::io::Result<()> {
        // we add segment_len to this check because we may be getting the
        // initial segment that is a bit behind where we left off before.
        trace!("Iter::read_segment lsn: {:?} cur_lsn: {:?}", lsn, self.cur_lsn);
        // TODO done? don't skip segments in SA, unify reuse_segment logic, remove from ordering consistently assert!(lsn >= offset, "lsn should never be less than the log offset");
        assert!(lsn + self.segment_len as Lsn >= self.cur_lsn);
        let cached_f = self.config.cached_file();
        let mut f = cached_f.borrow_mut();
        let segment_header = f.read_segment_header(offset)?;
        assert_eq!(offset % self.segment_len as Lsn, 0);
        assert_eq!(segment_header.lsn % self.segment_len as Lsn, 0);

        if segment_header.lsn != lsn {
            // this page was torn, nothing to read
            return Err(
                Error::new(ErrorKind::Other, "encountered torn segment"),
            );
        }

        let trailer_offset = offset + self.segment_len as LogID -
            SEG_TRAILER_LEN as LogID;
        let trailer_lsn = segment_header.lsn + self.segment_len as Lsn -
            SEG_TRAILER_LEN as Lsn;

        trace!("trying to read trailer from {}", trailer_offset);
        let segment_trailer = f.read_segment_trailer(trailer_offset);

        trace!("read segment header {:?}", segment_header);
        trace!("read segment trailer {:?}", segment_trailer);

        let trailer_lsn = segment_trailer.ok().and_then(|st| if st.ok &&
            st.lsn == trailer_lsn
        {
            Some(st.lsn)
        } else {
            None
        });

        self.trailer = trailer_lsn;
        self.cur_lsn = segment_header.lsn + SEG_HEADER_LEN as Lsn;
        self.segment_base = Some(offset);

        Ok(())
    }
}

fn valid_entry_offset(lid: LogID, segment_len: usize) -> bool {
    let seg_start = lid / segment_len as LogID * segment_len as LogID;

    let max_lid = seg_start + segment_len as LogID -
        SEG_TRAILER_LEN as LogID - MSG_HEADER_LEN as LogID;

    let min_lid = seg_start + SEG_HEADER_LEN as LogID;

    lid >= min_lid && lid <= max_lid
}
