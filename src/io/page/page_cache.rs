use std::io::{Read, Seek, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use coco::epoch::{Owned, Ptr, Scope, pin};

#[cfg(feature = "rayon")]
use rayon::prelude::*;

#[cfg(feature = "zstd")]
use zstd::block::{compress, decompress};

use super::*;

/// A lock-free pagecache which supports fragmented pages
/// for dramatically improving write throughput.
///
/// # Working with the `PageCache`
///
/// ```
/// extern crate sled;
/// extern crate coco;
///
/// use sled::Materializer;
///
/// use coco::epoch::pin;
///
/// pub struct TestMaterializer;
///
/// impl Materializer for TestMaterializer {
///     type PageFrag = String;
///     type Recovery = ();
///
///     fn merge(&self, frags: &[&String]) -> String {
///         let mut consolidated = String::new();
///         for frag in frags.into_iter() {
///             consolidated.push_str(&*frag);
///         }
///
///         consolidated
///     }
///
///     fn recover(&self, _: &String) -> Option<()> {
///         None
///     }
/// }
///
/// fn main() {
///     let path = "test_pagecache_doc.log";
///     let conf = sled::Config::default().path(path.to_owned());
///     let pc = sled::PageCache::new(TestMaterializer,
///                                   conf.clone());
///     pin(|scope| {
///         let (id, key) = pc.allocate(scope);
///
///         // The first item in a page should be set using replace,
///         // which signals that this is the beginning of a new
///         // page history, and that any previous items associated
///         // with this page should be forgotten.
///         let key = pc.replace(id, key, "a".to_owned(), scope).unwrap();
///
///         // Subsequent atomic updates should be added with link.
///         let key = pc.link(id, key, "b".to_owned(), scope).unwrap();
///         let _key = pc.link(id, key, "c".to_owned(), scope).unwrap();
///
///         // When getting a page, the provide `Materializer` is
///         // used to merge all pages together.
///         let (consolidated, _key) = pc.get(id, scope).unwrap();
///
///         assert_eq!(consolidated, "abc".to_owned());
///     });
///
///     drop(pc);
///     std::fs::remove_file(path).unwrap();
/// }
/// ```
pub struct PageCache<PM, P, R>
    where P: 'static + Send + Sync
{
    t: PM,
    config: Config,
    inner: Radix<Stack<CacheEntry<P>>>,
    max_pid: AtomicUsize,
    free: Arc<Stack<PageID>>,
    log: Log,
    lru: Lru,
    updates: AtomicUsize,
    last_snapshot: Mutex<Option<Snapshot<R>>>,
}

unsafe impl<PM, P, R> Send for PageCache<PM, P, R>
    where PM: Send + Sync,
          P: 'static + Send + Sync,
          R: Send
{
}

unsafe impl<PM, P, R> Sync for PageCache<PM, P, R>
    where PM: Send + Sync,
          P: 'static + Send + Sync,
          R: Send
{
}

impl<PM, P, R> Debug for PageCache<PM, P, R>
    where PM: Send + Sync,
          P: Debug + Send + Sync,
          R: Debug + Send
{
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        f.write_str(&*format!(
            "PageCache {{ max: {:?} free: {:?} }}\n",
            self.max_pid.load(SeqCst),
            self.free
        ))
    }
}

impl<PM, P, R> PageCache<PM, P, R>
    where PM: Materializer<PageFrag = P, Recovery = R>,
          PM: Send + Sync,
          P: 'static
                 + Debug
                 + Clone
                 + Serialize
                 + DeserializeOwned
                 + Send
                 + Sync,
          R: Debug + Clone + Serialize + DeserializeOwned + Send
{
    /// Instantiate a new `PageCache`.
    pub fn new(pm: PM, config: Config) -> PageCache<PM, P, R> {
        let cache_capacity = config.get_cache_capacity();
        let cache_shard_bits = config.get_cache_bits();
        let lru = Lru::new(cache_capacity, cache_shard_bits);

        PageCache {
            t: pm,
            config: config.clone(),
            inner: Radix::default(),
            max_pid: AtomicUsize::new(0),
            free: Arc::new(Stack::default()),
            log: Log::start_system(config),
            lru: lru,
            updates: AtomicUsize::new(0),
            last_snapshot: Mutex::new(None),
        }
    }

    /// Read updates from the log, apply them to our pagecache.
    pub fn recover(&mut self) -> Option<R> {
        // pull any existing snapshot off disk
        self.read_snapshot();

        // we call advance_snapshot here to "catch-up" the snapshot using the
        // logged updates before recovering from it. this allows us to reuse
        // the snapshot generation logic as initial log parsing logic. this is
        // also important for ensuring that we feed the provided `Materializer`
        // a single, linearized history, rather than going back in time
        // when generating a snapshot.
        self.advance_snapshot();

        // now we read it back in
        self.load_snapshot();

        let mu = &self.last_snapshot.lock().unwrap();

        let recovery = if let Some(ref snapshot) = **mu {
            snapshot.recovery.clone()
        } else {
            None
        };

        debug!(
            "recovery complete, returning recovery state to PageCache owner: {:?}",
            recovery
        );

        recovery
    }

    /// Create a new page, trying to reuse old freed pages if possible
    /// to maximize underlying `Radix` pointer density.
    pub fn allocate<'s>(&self, _: &'s Scope) -> (PageID, HPtr<'s, P>) {
        let pid = self.free.pop().unwrap_or_else(
            || self.max_pid.fetch_add(1, SeqCst),
        );
        // FIXME unwrap called on Err value
        // suspect: recovery issue?
        self.inner.insert(pid, Stack::default()).unwrap();

        // write info to log
        let prepend: LoggedUpdate<P> = LoggedUpdate {
            pid: pid,
            update: Update::Alloc,
        };
        let serialize_start = clock();
        let bytes = serialize(&prepend, Infinite).unwrap();
        M.serialize.measure(clock() - serialize_start);

        let (lsn, lid) = self.log.write(bytes);
        trace!("allocating pid {} at lsn {} lid {}", pid, lsn, lid);

        (pid, Ptr::null())
    }

    /// Free a particular page.
    pub fn free(&self, pid: PageID) {
        pin(|scope| {
            let deleted = self.inner.del(pid, scope);
            if deleted.is_none() {
                return;
            }

            // write info to log
            let prepend: LoggedUpdate<P> = LoggedUpdate {
                pid: pid,
                update: Update::Free,
            };
            let serialize_start = clock();
            let bytes = serialize(&prepend, Infinite).unwrap();
            M.serialize.measure(clock() - serialize_start);

            let res = self.log.reserve(bytes);

            // add pid to free stack to reduce fragmentation over time
            unsafe {
                let cas_key = deleted.unwrap().deref().head(scope);

                let lsn = res.lsn();
                let lid = res.lid();

                self.log.with_sa(|sa| {
                    sa.mark_replace(
                        pid,
                        lsn,
                        lids_from_stack(cas_key, scope),
                        lid,
                    )
                });
            }

            // NB complete must happen AFTER calls to SA, because
            // when the iobuf's n_writers hits 0, we may transition
            // the segment to inactive, resulting in a race otherwise.
            res.complete();

            let pd = Owned::new(PidDropper(pid, self.free.clone()));
            let ptr = pd.into_ptr(scope);
            unsafe {
                scope.defer_drop(ptr);
                scope.flush();
            }
        });
    }

    /// Try to retrieve a page by its logical ID.
    pub fn get<'s>(
        &self,
        pid: PageID,
        scope: &'s Scope,
    ) -> Option<(PM::PageFrag, HPtr<'s, P>)> {
        let stack_ptr = self.inner.get(pid, scope);
        if stack_ptr.is_none() {
            return None;
        }

        let stack_ptr = stack_ptr.unwrap();

        let head = unsafe { stack_ptr.deref().head(scope) };

        self.page_in(pid, head, stack_ptr, scope)
    }

    fn page_out<'s>(&self, to_evict: Vec<PageID>, scope: &'s Scope) {
        let start = clock();
        for pid in to_evict {
            let stack_ptr = self.inner.get(pid, scope);
            if stack_ptr.is_none() {
                continue;
            }

            let stack_ptr = stack_ptr.unwrap();

            let head = unsafe { stack_ptr.deref().head(scope) };
            let stack_iter = StackIter::from_ptr(head, scope);

            let mut cache_entries: Vec<CacheEntry<P>> =
                stack_iter.map(|ptr| (*ptr).clone()).collect();

            // ensure the last entry is a Flush
            let last = cache_entries.pop().map(|last_ce| match last_ce {
                CacheEntry::MergedResident(_, lsn, lid) |
                CacheEntry::Resident(_, lsn, lid) |
                CacheEntry::Flush(lsn, lid) => {
                    // NB stabilize the most recent LSN before
                    // paging out! This SHOULD very rarely block...
                    // TODO measure to make sure
                    self.log.make_stable(lsn);
                    CacheEntry::Flush(lsn, lid)
                }
                CacheEntry::PartialFlush(_, _) => {
                    panic!("got PartialFlush at end of stack...")
                }
            });

            if last.is_none() {
                M.page_out.measure(clock() - start);
                return;
            }

            let mut new_stack = Vec::with_capacity(cache_entries.len() + 1);
            for entry in cache_entries {
                match entry {
                    CacheEntry::PartialFlush(lsn, lid) |
                    CacheEntry::MergedResident(_, lsn, lid) |
                    CacheEntry::Resident(_, lsn, lid) => {
                        new_stack.push(CacheEntry::PartialFlush(lsn, lid));
                    }
                    CacheEntry::Flush(_, _) => {
                        panic!("got Flush in middle of stack...")
                    }
                }
            }
            new_stack.push(last.unwrap());
            let node = node_from_frag_vec(new_stack);

            debug_delay();
            unsafe {
                if stack_ptr
                    .deref()
                    .cas(head, node.into_ptr(scope), scope)
                    .is_err()
                {}
            }
        }
        M.page_out.measure(clock() - start);
    }

    fn pull(&self, lsn: Lsn, lid: LogID) -> P {
        trace!("pulling lsn {} lid {} from disk", lsn, lid);
        let start = clock();
        let bytes = match self.log.read(lsn, lid).map_err(|_| ()) {
            Ok(LogRead::Flush(_lsn, data, _len)) => data,
            _ => panic!("read invalid data at lid {}", lid),
        };

        let deserialize_start = clock();
        let logged_update = deserialize::<LoggedUpdate<P>>(&*bytes)
            .map_err(|_| ())
            .expect("failed to deserialize data");
        M.deserialize.measure(clock() - deserialize_start);

        M.pull.measure(clock() - start);
        match logged_update.update {
            Update::Compact(page_frag) |
            Update::Append(page_frag) => page_frag,
            _ => panic!("non-append/compact found in pull"),
        }
    }

    fn page_in<'s>(
        &self,
        pid: PageID,
        mut head: Ptr<'s, ds::stack::Node<CacheEntry<P>>>,
        stack_ptr: Ptr<'s, ds::stack::Stack<CacheEntry<P>>>,
        scope: &'s Scope,
    ) -> Option<(PM::PageFrag, HPtr<'s, P>)> {
        let start = clock();
        let stack_iter = StackIter::from_ptr(head, scope);

        let mut to_merge = vec![];
        let mut merged_resident = false;
        let mut lids = vec![];
        let mut fix_up_length = 0;

        for cache_entry_ptr in stack_iter {
            match *cache_entry_ptr {
                CacheEntry::Resident(ref page_frag, lsn, lid) => {
                    if !merged_resident {
                        to_merge.push(page_frag);
                    }
                    lids.push((lsn, lid));
                }
                CacheEntry::MergedResident(ref page_frag, lsn, lid) => {
                    if lids.is_empty() {
                        // Short circuit merging and fix-up if we only
                        // have one frag.
                        return Some((page_frag.clone(), head));
                    }
                    if !merged_resident {
                        to_merge.push(page_frag);
                        merged_resident = true;
                        fix_up_length = lids.len();
                    }
                    lids.push((lsn, lid));
                }
                CacheEntry::PartialFlush(lsn, lid) |
                CacheEntry::Flush(lsn, lid) => {
                    lids.push((lsn, lid));
                }
            }
        }

        if lids.is_empty() {
            M.page_in.measure(clock() - start);
            return None;
        }

        let mut fetched = Vec::with_capacity(lids.len());

        // Did not find a previously merged value in memory,
        // may need to go to disk.
        if !merged_resident {
            let to_pull = &lids[to_merge.len()..];

            #[cfg(feature = "rayon")]
            {
                let mut pulled: Vec<P> = to_pull
                    .par_iter()
                    .map(|&(lsn, lid)| self.pull(lsn, lid))
                    .collect();
                fetched.append(&mut pulled);
            }

            #[cfg(not(feature = "rayon"))]
            for &(lsn, lid) in to_pull {
                fetched.push(self.pull(lsn, lid));
            }
        }

        let combined: Vec<&P> = to_merge
            .iter()
            .cloned()
            .chain(fetched.iter())
            .rev()
            .collect();

        let before_merge = clock();
        let merged = self.t.merge(&*combined);
        M.merge_page.measure(clock() - before_merge);

        let size = std::mem::size_of_val(&merged);
        let to_evict = self.lru.accessed(pid, size);
        trace!("accessed pid {} -> paging out pid {:?}", pid, to_evict);
        self.page_out(to_evict, scope);

        if lids.len() > self.config.get_page_consolidation_threshold() {
            trace!("consolidating pid {} with len {}!", pid, lids.len());
            match self.replace_recurse_once(
                pid,
                head,
                merged.clone(),
                scope,
                true,
            ) {
                Ok(new_head) => head = new_head,
                Err(None) => return None,
                _ => (),
            }
        } else if !fetched.is_empty() ||
                   fix_up_length >= self.config.get_cache_fixup_threshold()
        {
            trace!(
                "fixing up pid {} with {} traversed frags",
                pid,
                fix_up_length
            );
            let mut new_entries = Vec::with_capacity(lids.len());

            let (head_lsn, head_lid) = lids.remove(0);
            let head_entry =
                CacheEntry::MergedResident(merged.clone(), head_lsn, head_lid);
            new_entries.push(head_entry);

            let mut tail = if let Some((lsn, lid)) = lids.pop() {
                Some(CacheEntry::Flush(lsn, lid))
            } else {
                None
            };

            for (lsn, lid) in lids {
                new_entries.push(CacheEntry::PartialFlush(lsn, lid));
            }

            if let Some(tail) = tail.take() {
                new_entries.push(tail);
            }

            let node = node_from_frag_vec(new_entries);

            debug_delay();
            let res = unsafe {
                stack_ptr.deref().cas(head, node.into_ptr(scope), scope)
            };
            if let Ok(new_head) = res {
                head = new_head;
            } else {
                // NB explicitly DON'T update head, as our witnessed
                // entries do NOT contain the latest state. This
                // may not matter to callers who only care about
                // reading, but maybe we should signal that it's
                // out of date for those who page_in in an attempt
                // to modify!
            }
        }

        M.page_in.measure(clock() - start);

        Some((merged, head))
    }

    /// Replace an existing page with a different set of `PageFrag`s.
    /// Returns `Ok(new_key)` if the operation was successful. Returns
    /// `Err(None)` if the page no longer exists. Returns `Err(Some(actual_key))`
    /// if the atomic swap fails.
    pub fn replace<'s>(
        &self,
        pid: PageID,
        old: HPtr<'s, P>,
        new: P,
        scope: &'s Scope,
    ) -> Result<HPtr<'s, P>, Option<HPtr<'s, P>>> {
        self.replace_recurse_once(pid, old, new, scope, false)
    }

    fn replace_recurse_once<'s>(
        &self,
        pid: PageID,
        old: HPtr<'s, P>,
        new: P,
        scope: &'s Scope,
        recursed: bool,
    ) -> Result<HPtr<'s, P>, Option<HPtr<'s, P>>> {
        trace!("replacing pid {}", pid);
        let stack_ptr = self.inner.get(pid, scope);
        if stack_ptr.is_none() {
            return Err(None);
        }
        let stack_ptr = stack_ptr.unwrap();

        let replace: LoggedUpdate<P> = LoggedUpdate {
            pid: pid,
            update: Update::Compact(new.clone()),
        };
        let serialize_start = clock();
        let bytes = serialize(&replace, Infinite).unwrap();
        M.serialize.measure(clock() - serialize_start);
        let log_reservation = self.log.reserve(bytes);
        let lsn = log_reservation.lsn();
        let lid = log_reservation.lid();

        let cache_entry = CacheEntry::MergedResident(new, lsn, lid);

        let node = node_from_frag_vec(vec![cache_entry]).into_ptr(scope);

        debug_delay();
        let result = unsafe { stack_ptr.deref().cas(old.clone(), node, scope) };

        if result.is_ok() {
            let lid = log_reservation.lid();
            let lsn = log_reservation.lsn();
            let lids = lids_from_stack(old, scope);

            let to_clean = self.log.with_sa(|sa| {
                sa.mark_replace(pid, lsn, lids, lid);
                if recursed { None } else { sa.clean(Some(pid)) }
            });
            if let Some(to_clean) = to_clean {
                assert_ne!(pid, to_clean);
                if let Some((page, key)) = self.get(to_clean, scope) {
                    let _ = self.replace_recurse_once(
                        to_clean,
                        key,
                        page,
                        scope,
                        true,
                    );
                }
            }

            // NB complete must happen AFTER calls to SA, because
            // when the iobuf's n_writers hits 0, we may transition
            // the segment to inactive, resulting in a race otherwise.
            log_reservation.complete();

            let count = self.updates.fetch_add(1, SeqCst) + 1;
            let should_snapshot =
                count % self.config.get_snapshot_after_ops() == 0;
            if should_snapshot {
                self.advance_snapshot();
            }
        } else {
            log_reservation.abort();
        }

        result.map_err(|e| Some(e))
    }


    /// Try to atomically add a `PageFrag` to the page.
    /// Returns `Ok(new_key)` if the operation was successful. Returns
    /// `Err(None)` if the page no longer exists. Returns `Err(Some(actual_key))`
    /// if the atomic append fails.
    pub fn link<'s>(
        &self,
        pid: PageID,
        old: HPtr<'s, P>,
        new: P,
        scope: &'s Scope,
    ) -> Result<HPtr<'s, P>, Option<HPtr<'s, P>>> {
        let stack_ptr = self.inner.get(pid, scope);
        if stack_ptr.is_none() {
            return Err(None);
        }
        let stack_ptr = stack_ptr.unwrap();

        let prepend: LoggedUpdate<P> = LoggedUpdate {
            pid: pid,
            update: if old.is_null() {
                Update::Compact(new.clone())
            } else {
                Update::Append(new.clone())
            },
        };
        let serialize_start = clock();
        let bytes = serialize(&prepend, Infinite).unwrap();
        M.serialize.measure(clock() - serialize_start);
        let log_reservation = self.log.reserve(bytes);
        let lsn = log_reservation.lsn();
        let lid = log_reservation.lid();

        let cache_entry = CacheEntry::Resident(new, lsn, lid);

        let result = unsafe { stack_ptr.deref().cap(old, cache_entry, scope) };

        if result.is_err() {
            log_reservation.abort();
        } else {
            let to_clean = self.log.with_sa(|sa| {
                sa.mark_link(pid, lsn, lid);
                sa.clean(None)
            });
            if let Some(to_clean) = to_clean {
                if let Some((page, key)) = self.get(to_clean, scope) {
                    let _ = self.replace_recurse_once(
                        to_clean,
                        key,
                        page,
                        scope,
                        true,
                    );
                }
            }

            // NB complete must happen AFTER calls to SA, because
            // when the iobuf's n_writers hits 0, we may transition
            // the segment to inactive, resulting in a race otherwise.
            log_reservation.complete();

            let count = self.updates.fetch_add(1, SeqCst) + 1;
            let should_snapshot =
                count % self.config.get_snapshot_after_ops() == 0;
            if should_snapshot {
                self.advance_snapshot();
            }
        }

        result.map_err(|e| Some(e))
    }

    fn advance_snapshot(&self) {
        let start = clock();

        self.log.flush();

        let snapshot_opt_res = self.last_snapshot.try_lock();
        if snapshot_opt_res.is_err() {
            // some other thread is snapshotting
            warn!(
                "snapshot skipped because previous attempt \
                  appears not to have completed"
            );
            M.advance_snapshot.measure(clock() - start);
            return;
        }
        let mut snapshot_opt = snapshot_opt_res.unwrap();
        let mut snapshot =
            snapshot_opt.take().unwrap_or_else(Snapshot::default);

        // we disable rewriting so that our log becomes append-only,
        // allowing us to iterate through it without corrupting ourselves.
        self.log.with_sa(|sa| sa.pause_rewriting());

        trace!("building on top of old snapshot: {:?}", snapshot);

        debug!(
            "snapshot starting from offset {} to the segment containing ~{}",
            snapshot.max_lsn,
            self.log.stable_offset(),
        );

        let io_buf_size = self.config.get_io_buf_size();

        let mut recovery = snapshot.recovery.take();
        let mut max_lsn = snapshot.max_lsn;
        let start_lsn = max_lsn - (max_lsn % io_buf_size as Lsn);
        let stop_lsn = self.log.stable_offset();

        let mut last_segment = None;

        for (lsn, log_id, bytes) in self.log.iter_from(start_lsn) {
            if stop_lsn > 0 && lsn > stop_lsn {
                // we've gone past the known-stable offset.
                break;
            }
            let segment_lsn = lsn / io_buf_size as Lsn * io_buf_size as Lsn;

            trace!(
                "in advance_snapshot looking at item: segment lsn {} lsn {} lid {}",
                segment_lsn,
                lsn,
                log_id
            );

            if lsn <= max_lsn {
                // don't process alread-processed Lsn's.
                trace!(
                    "continuing in advance_snapshot, lsn {} log_id {} max_lsn {}",
                    lsn,
                    log_id,
                    max_lsn
                );
                continue;
            }

            assert!(lsn > max_lsn);
            max_lsn = lsn;

            let idx = log_id as usize / io_buf_size;
            if snapshot.segments.len() < idx + 1 {
                snapshot.segments.resize(idx + 1, log::Segment::default());
            }

            assert_eq!(
                segment_lsn / io_buf_size as Lsn * io_buf_size as Lsn,
                segment_lsn,
                "segment lsn is unaligned! fix above lsn statement..."
            );

            // unwrapping this because it's already passed the crc check
            // in the log iterator
            trace!("trying to deserialize buf for lid {} lsn {}", log_id, lsn);
            let deserialization = deserialize::<LoggedUpdate<P>>(&*bytes);

            if let Err(e) = deserialization {
                error!(
                    "failed to deserialize buffer for item in log: lsn {} \
                    lid {}: {:?}",
                    lsn,
                    log_id,
                    e
                );
                continue;
            }

            let prepend = deserialization.unwrap();

            if prepend.pid >= snapshot.max_pid {
                snapshot.max_pid = prepend.pid + 1;
            }

            snapshot.segments[idx].recovery_ensure_initialized(segment_lsn);

            let last_idx = *last_segment.get_or_insert(idx);
            if last_idx != idx {
                // if we have moved to a new segment, mark the previous one
                // as inactive.
                trace!(
                    "PageCache recovery setting segment {} to inactive",
                    log_id
                );
                snapshot.segments[last_idx].active_to_inactive(
                    segment_lsn,
                    true,
                );
                if snapshot.segments[last_idx].is_empty() {
                    trace!(
                        "PageCache recovery setting segment {} to draining",
                        log_id
                    );
                    snapshot.segments[last_idx].inactive_to_draining(
                        segment_lsn,
                    );
                }
            }
            last_segment = Some(idx);

            match prepend.update {
                Update::Append(partial_page) => {
                    // Because we rewrite pages over time, we may have relocated
                    // a page's initial Compact to a later segment. We should skip
                    // over pages here unless we've encountered a Compact or Alloc
                    // for them.
                    if let Some(lids) = snapshot.pt.get_mut(&prepend.pid) {
                        trace!(
                            "append of pid {} at lid {} lsn {}",
                            prepend.pid,
                            log_id,
                            lsn
                        );

                        snapshot.segments[idx].insert_pid(
                            prepend.pid,
                            segment_lsn,
                        );

                        let r = self.t.recover(&partial_page);
                        if r.is_some() {
                            recovery = r;
                        }

                        lids.push((lsn, log_id));
                    }
                }
                Update::Compact(partial_page) => {
                    trace!(
                        "compact of pid {} at lid {} lsn {}",
                        prepend.pid,
                        log_id,
                        lsn
                    );
                    if let Some(lids) = snapshot.pt.remove(&prepend.pid) {
                        for (_lsn, old_lid) in lids {
                            let old_idx = old_lid as usize / io_buf_size;
                            if old_idx == idx {
                                // don't remove pid if it's still there
                                continue;
                            }
                            let old_segment = &mut snapshot.segments[old_idx];

                            old_segment.remove_pid(prepend.pid, segment_lsn);
                        }
                    }

                    snapshot.segments[idx].insert_pid(prepend.pid, segment_lsn);

                    let r = self.t.recover(&partial_page);
                    if r.is_some() {
                        recovery = r;
                    }

                    snapshot.pt.insert(prepend.pid, vec![(lsn, log_id)]);
                }
                Update::Free => {
                    trace!(
                        "del of pid {} at lid {} lsn {}",
                        prepend.pid,
                        log_id,
                        lsn
                    );
                    if let Some(lids) = snapshot.pt.remove(&prepend.pid) {
                        // this could fail if our Alloc was nuked
                        for (_lsn, old_lid) in lids {
                            let old_idx = old_lid as usize / io_buf_size;
                            if old_idx == idx {
                                // don't remove pid if it's still there
                                continue;
                            }
                            let old_segment = &mut snapshot.segments[old_idx];
                            old_segment.remove_pid(prepend.pid, segment_lsn);
                        }
                    }

                    snapshot.segments[idx].insert_pid(prepend.pid, segment_lsn);

                    snapshot.free.push(prepend.pid);
                }
                Update::Alloc => {
                    trace!(
                        "alloc of pid {} at lid {} lsn {}",
                        prepend.pid,
                        log_id,
                        lsn
                    );

                    snapshot.pt.insert(prepend.pid, vec![]);
                    snapshot.free.retain(|&pid| pid != prepend.pid);
                    snapshot.segments[idx].insert_pid(prepend.pid, segment_lsn);
                }
            }
        }

        snapshot.free.sort();
        snapshot.free.reverse();
        snapshot.max_lsn = max_lsn;
        snapshot.recovery = recovery;

        self.write_snapshot(&snapshot);

        trace!("generated new snapshot: {:?}", snapshot);

        self.log.with_sa(|sa| sa.resume_rewriting());

        // NB replacing the snapshot must come after the resume_rewriting call
        // otherwise we create a race condition where we corrupt an in-progress
        // snapshot generating iterator.
        *snapshot_opt = Some(snapshot);

        M.advance_snapshot.measure(clock() - start);
    }

    fn write_snapshot(&self, snapshot: &Snapshot<R>) {
        let raw_bytes = serialize(&snapshot, Infinite).unwrap();

        #[cfg(feature = "zstd")]
        let bytes = if self.config.get_use_compression() {
            compress(&*raw_bytes, 5).unwrap()
        } else {
            raw_bytes
        };

        #[cfg(not(feature = "zstd"))]
        let bytes = raw_bytes;

        let crc64: [u8; 8] = unsafe { std::mem::transmute(crc64(&*bytes)) };

        let prefix = self.config.snapshot_prefix();

        let path_1 = format!("{}.{}.in___motion", prefix, snapshot.max_lsn);
        let path_2 = format!("{}.{}", prefix, snapshot.max_lsn);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(&path_1)
            .unwrap();

        // write the snapshot bytes, followed by a crc64 checksum at the end
        f.write_all(&*bytes).unwrap();
        f.write_all(&crc64).unwrap();
        f.sync_all().unwrap();
        drop(f);

        trace!("wrote snapshot to {}", path_1);

        std::fs::rename(path_1, &path_2).expect("failed to write snapshot");

        trace!("renamed snapshot to {}", path_2);

        // clean up any old snapshots
        let candidates = self.config.get_snapshot_files();
        for path in candidates {
            let path_str =
                Path::new(&path).file_name().unwrap().to_str().unwrap();
            if !path_2.ends_with(&*path_str) {
                debug!("removing old snapshot file {:?}", path);

                if let Err(_e) = std::fs::remove_file(&path) {
                    warn!(
                        "failed to remove old snapshot file, maybe snapshot race? {}",
                        _e
                    );
                }
            }
        }
    }

    fn read_snapshot(&self) {
        let mut candidates = self.config.get_snapshot_files();
        if candidates.is_empty() {
            info!("no previous snapshot found");
            return;
        }

        candidates.sort_by_key(
            |path| std::fs::metadata(path).unwrap().created().unwrap(),
        );

        let path = candidates.pop().unwrap();

        let mut f = std::fs::OpenOptions::new().read(true).open(&path).unwrap();

        let mut buf = vec![];
        f.read_to_end(&mut buf).unwrap();
        let len = buf.len();
        buf.split_off(len - 8);

        let mut crc_expected_bytes = [0u8; 8];
        f.seek(std::io::SeekFrom::End(-8)).unwrap();
        f.read_exact(&mut crc_expected_bytes).unwrap();

        let crc_expected: u64 =
            unsafe { std::mem::transmute(crc_expected_bytes) };
        let crc_actual = crc64(&*buf);

        if crc_expected != crc_actual {
            panic!("crc for snapshot file {:?} failed!", path);
        }

        #[cfg(feature = "zstd")]
        let bytes = if self.config.get_use_compression() {
            decompress(&*buf, self.config.get_io_buf_size()).unwrap()
        } else {
            buf
        };

        #[cfg(not(feature = "zstd"))]
        let bytes = buf;

        let snapshot = deserialize::<Snapshot<R>>(&*bytes).unwrap();

        let mut mu = self.last_snapshot.lock().unwrap();
        *mu = Some(snapshot);
    }

    fn load_snapshot(&mut self) {
        let mu = self.last_snapshot.lock().unwrap();
        if let Some(ref snapshot) = *mu {
            self.max_pid.store(snapshot.max_pid, SeqCst);

            let mut free = snapshot.free.clone();
            free.sort();
            free.reverse();
            for pid in free {
                trace!("adding {} to free during load_snapshot", pid);
                self.free.push(pid);
            }

            for (pid, lids) in &snapshot.pt {
                trace!("loading pid {} in load_snapshot", pid);

                let mut lids = lids.clone();
                let stack = Stack::default();

                if !lids.is_empty() {
                    let (base_lsn, base_lid) = lids.remove(0);
                    stack.push(CacheEntry::Flush(base_lsn, base_lid));

                    for (lsn, lid) in lids {
                        stack.push(CacheEntry::PartialFlush(lsn, lid));
                    }
                }

                self.inner.insert(*pid, stack).unwrap();
            }

            self.log.with_sa(
                |sa| sa.initialize_from_segments(snapshot.segments.clone()),
            );
        } else {
            panic!("no snapshot present in load_snapshot");
        }
    }
}

fn lids_from_stack<'s, P: Send + Sync>(
    head_ptr: HPtr<'s, P>,
    scope: &'s Scope,
) -> Vec<LogID> {
    // generate a list of the old log ID's
    let stack_iter = StackIter::from_ptr(head_ptr, scope);

    let mut lids = vec![];
    for cache_entry_ptr in stack_iter {
        match *cache_entry_ptr {
            CacheEntry::Resident(_, _, ref lid) |
            CacheEntry::MergedResident(_, _, ref lid) |
            CacheEntry::PartialFlush(_, ref lid) |
            CacheEntry::Flush(_, ref lid) => {
                lids.push(*lid);
            }
        }
    }
    lids
}
