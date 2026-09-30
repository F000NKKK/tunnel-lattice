//! Fixed-capacity receive-buffer pool: one zeroed slab carved into equally
//! sized slots, handed out as [`PacketBuf`] offset views.
//!
//! All `unsafe` in this crate, and every invariant it relies on, lives in
//! this module:
//!
//! 1. `alloc_zeroed`/`dealloc` of the slab with its stored `Layout`
//!    ([`Slab`]).
//! 2. The `64`-byte alignment offset from the raw allocation to `base`.
//! 3. `slice::from_raw_parts[_mut]` over exactly one slot or one view,
//!    always derived from the raw slab pointer; the whole slab is never
//!    reborrowed, so several disjoint slot slices may be live at once.
//! 4. `unsafe impl Send/Sync for Slab`.
//!
//! **Ownership invariant.** From the moment a slot index is popped off the
//! free list until it is pushed back, exactly one [`SlotGuard`] or one
//! [`PacketBuf`] owns it. The handoff between the two is safe code
//! (`Option::take` of the pool `Arc`).
//!
//! **Containment invariant.** For every live view, `slot_base <= off` and
//! `off + len <= slot_base + buf_len`, where `slot_base = off / stride *
//! stride`. Every function here that creates or reshapes a view
//! ([`SlotGuard::into_buf`], [`PacketBuf::advance`], [`PacketBuf::truncate`])
//! enforces it itself, so no caller outside this module can build a view
//! that crosses into another slot.
//!
//! **Foreign code** (any `Waker` clone, wake, or drop) never runs while the
//! pool mutex is held; only `Waker::will_wake` does. Every foreign wake or
//! drop that runs outside the consumer's own poll is wrapped in
//! `catch_unwind` and its payload is discarded, because release runs inside
//! destructors, where an escaping panic would abort the process or skip the
//! remaining wakers.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::fmt;
use std::future::Future;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::ptr::NonNull;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use tunnel_lattice_core::{Error, Result};

/// Slot and slab alignment: one cache line.
const SLOT_ALIGN: usize = 64;

/// `align_up(buf_len + 1, 64)`, or `None` for `buf_len == 0` or overflow.
///
/// The extra byte keeps `off / stride` unambiguous after
/// `advance(len)` on a packet that fills the whole slot.
fn stride_for(buf_len: usize) -> Option<usize> {
    if buf_len == 0 {
        return None;
    }
    Some(buf_len.checked_add(1)?.checked_add(SLOT_ALIGN - 1)? & !(SLOT_ALIGN - 1))
}

/// Checked slab sizing: `(stride, total, layout)`.
///
/// Rejects `slots == 0`, `buf_len == 0`, any arithmetic overflow, a slab of
/// more than `u32::MAX` bytes (offsets are stored as `u32`), and a total
/// size above `isize::MAX` (`Layout` rejects it).
fn layout_for(slots: usize, buf_len: usize) -> Option<(usize, usize, Layout)> {
    if slots == 0 {
        return None;
    }
    let stride = stride_for(buf_len)?;
    let size = slots.checked_mul(stride)?;
    if size > u32::MAX as usize {
        return None;
    }
    // `SLOT_ALIGN - 1` bytes of slack for aligning `base` by hand.
    let total = size.checked_add(SLOT_ALIGN - 1)?;
    let layout = Layout::from_size_align(total, 1).ok()?;
    Some((stride, total, layout))
}

/// The number of slots [`PacketPool::with_buf_len`] picks.
fn default_slots(stride: usize) -> usize {
    (PacketPool::DEFAULT_MAX_BYTES / stride).clamp(1, PacketPool::DEFAULT_SLOTS)
}

/// The zeroed slab allocation. Owns it; frees it on drop.
struct Slab {
    /// Start of the allocation, as returned by `alloc_zeroed`.
    raw: NonNull<u8>,
    /// `raw` rounded up to [`SLOT_ALIGN`]; slot `i` starts at
    /// `base + i * stride`.
    base: NonNull<u8>,
    /// The layout `raw` was allocated with.
    layout: Layout,
}

impl Slab {
    /// Allocates `layout` zeroed. Aborts through `handle_alloc_error` if the
    /// allocation fails.
    fn new(layout: Layout) -> Self {
        debug_assert!(layout.size() >= SLOT_ALIGN);
        // SAFETY: `layout_for` only builds layouts of at least
        // `stride + 63 >= 127` bytes, so the size is non-zero.
        let raw = unsafe { alloc_zeroed(layout) };
        let Some(raw) = NonNull::new(raw) else {
            handle_alloc_error(layout)
        };
        let pad = raw.as_ptr().addr().wrapping_neg() & (SLOT_ALIGN - 1);
        // SAFETY: `pad <= 63` and the allocation is `slots * stride + 63`
        // bytes with `slots * stride >= 64`, so `raw + pad` is in bounds of
        // the same allocation.
        let base = unsafe { raw.add(pad) };
        Self { raw, base, layout }
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        // SAFETY: `raw` was returned by `alloc_zeroed(self.layout)` in
        // `Slab::new` and is freed only here, once.
        unsafe { dealloc(self.raw.as_ptr(), self.layout) }
    }
}

// SAFETY: `Slab` is a uniquely owned heap allocation with no thread
// affinity. The bytes behind it are only accessed through slot views whose
// exclusivity is guaranteed by the free list (one owner per popped index),
// so sharing the pointer between threads cannot create a data race.
unsafe impl Send for Slab {}
// SAFETY: `&Slab` exposes no access to the bytes by itself; see `Send`.
unsafe impl Sync for Slab {}

/// Free-list and waiter state, guarded by `PoolInner::state`.
///
/// Every change under the lock is a step that cannot stop half-way: a push
/// or pop within reserved capacity, a counter step, an entry replace or
/// take, or a `mem::replace`/`mem::take`.
struct FreeState {
    /// Free slot indices. Capacity is `slots` and never grows.
    free: Vec<u32>,
    /// Registered async acquirers, keyed by `(epoch, index)`. `None` is a
    /// tombstone left by an acquirer that took its own entry back.
    waiters: Vec<Option<Waker>>,
    /// The second waiter buffer, swapped in when `waiters` is drained.
    /// While `draining` is set the draining release holds it instead, and
    /// this is an empty `Vec` with no allocation.
    spare: Vec<Option<Waker>>,
    /// Number of `Some` entries in `waiters`.
    live: usize,
    /// Bumped whenever `waiters` is drained or compacted, which
    /// invalidates every outstanding `(epoch, index)` key.
    epoch: u64,
    /// Threads parked in `acquire_blocking`.
    blocked: u32,
    /// A release is waking a drained buffer outside the lock.
    draining: bool,
    /// Another release saw live waiters while `draining` was set; the
    /// draining release drains again before it returns.
    redrain: bool,
}

impl FreeState {
    /// Swaps the live waiters out for the spare buffer.
    fn begin_drain(&mut self) -> Vec<Option<Waker>> {
        let spare = mem::take(&mut self.spare);
        let drained = mem::replace(&mut self.waiters, spare);
        self.epoch = self.epoch.wrapping_add(1);
        self.live = 0;
        self.draining = true;
        drained
    }
}

/// Shared pool state. Freed (slab included) when the last `PacketPool`,
/// `PacketBuf`, `SlotGuard`, or pending acquire drops.
pub(crate) struct PoolInner {
    slab: Slab,
    stride: usize,
    buf_len: usize,
    slots: usize,
    state: Mutex<FreeState>,
    cv: Condvar,
    #[cfg(test)]
    hooks: Option<Arc<TestHooks>>,
}

impl PoolInner {
    /// Poison-tolerant lock: no code path panics while holding it, and a
    /// poisoned state is still consistent (see [`FreeState`]).
    fn lock(&self) -> MutexGuard<'_, FreeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Pointer to byte `off` of the aligned slab. Computing it is safe;
    /// dereferencing it relies on the containment invariant.
    fn view_ptr(&self, off: usize) -> *mut u8 {
        self.slab.base.as_ptr().wrapping_add(off)
    }

    fn notify_one(&self) {
        self.cv.notify_one();
        #[cfg(test)]
        if let Some(hooks) = &self.hooks {
            hooks.notify_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Returns slot `idx` to the free list and wakes whoever waits for one.
    fn release(&self, idx: u32) {
        let mut st = self.lock();
        #[cfg(test)]
        assert!(
            (idx as usize) < self.slots && !st.free.contains(&idx),
            "slot {idx} released twice or out of range"
        );
        // Never reallocates: capacity is `slots` and an index is pushed
        // only by its single owner.
        st.free.push(idx);
        let drained = if st.live > 0 {
            if st.draining {
                st.redrain = true;
                None
            } else {
                Some(st.begin_drain())
            }
        } else {
            if !st.waiters.is_empty() {
                // Only tombstones: compact them in place so the next release
                // does not take the slow path for nothing.
                st.waiters.clear();
                st.epoch = st.epoch.wrapping_add(1);
            }
            None
        };
        let need_notify = st.blocked > 0;
        drop(st);

        if need_notify {
            self.notify_one();
        }
        if let Some(drained) = drained {
            self.finish_drain(drained);
        }
    }

    /// Slow path of [`Self::release`]: wakes a drained buffer outside the
    /// lock, re-drains while overlapping releases asked for it, then puts
    /// the (now empty) buffer back as `spare`. Only two waiter buffers ever
    /// exist, so this never allocates.
    fn finish_drain(&self, mut drained: Vec<Option<Waker>>) {
        loop {
            wake_all(&mut drained);
            #[cfg(test)]
            self.hook_release_between_unlock_and_relock();

            let mut st = self.lock();
            if st.redrain && st.live > 0 {
                st.redrain = false;
                drained = mem::replace(&mut st.waiters, drained);
                st.epoch = st.epoch.wrapping_add(1);
                st.live = 0;
                continue;
            }
            st.redrain = false;
            st.draining = false;
            // The current `spare` is the empty, unallocated `Vec` left by
            // `begin_drain`; replacing it frees nothing.
            st.spare = drained;
            return;
        }
    }

    #[cfg(test)]
    fn hook_release_between_unlock_and_relock(&self) {
        if let Some(hooks) = &self.hooks {
            let hook = hooks
                .release_between_unlock_and_relock
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }
}

#[cfg(test)]
impl Drop for PoolInner {
    fn drop(&mut self) {
        if let Some(hooks) = &self.hooks {
            hooks.inner_dropped.store(true, Ordering::SeqCst);
        }
    }
}

/// Wakes and drops every drained waker, each under `catch_unwind`. Leaves
/// `drained` empty with its capacity intact.
fn wake_all(drained: &mut Vec<Option<Waker>>) {
    for waker in drained.drain(..).flatten() {
        let _ = catch_unwind(AssertUnwindSafe(move || waker.wake()));
    }
}

/// Drops a foreign waker outside the consumer's poll without letting a
/// panic escape.
fn drop_guarded(waker: Waker) {
    let _ = catch_unwind(AssertUnwindSafe(move || drop(waker)));
}

/// Stores `waker` as the acquirer's single entry. Returns the waker to drop
/// after unlocking (a displaced one, or `waker` itself if the stored one
/// already wakes the same task).
fn register(st: &mut FreeState, entry: &mut Option<(u64, usize)>, waker: Waker) -> Option<Waker> {
    if let Some((epoch, idx)) = *entry
        && epoch == st.epoch
        && let Some(slot) = st.waiters.get_mut(idx)
    {
        return match slot {
            Some(old) if old.will_wake(&waker) => Some(waker),
            Some(old) => Some(mem::replace(old, waker)),
            None => {
                *slot = Some(waker);
                st.live += 1;
                None
            }
        };
    }
    let idx = match st.waiters.iter().position(Option::is_none) {
        Some(idx) => {
            st.waiters[idx] = Some(waker);
            idx
        }
        None => {
            st.waiters.push(Some(waker));
            st.waiters.len() - 1
        }
    };
    st.live += 1;
    *entry = Some((st.epoch, idx));
    None
}

/// Takes the acquirer's own entry back, if it is still current.
fn take_entry(st: &mut FreeState, entry: &mut Option<(u64, usize)>) -> Option<Waker> {
    let (epoch, idx) = entry.take()?;
    if epoch != st.epoch {
        return None;
    }
    let waker = st.waiters.get_mut(idx)?.take();
    if waker.is_some() {
        st.live -= 1;
    }
    waker
}

/// A fixed-capacity pool of equally sized receive slots carved from one
/// slab that is allocated once.
///
/// Cloning is cheap (one `Arc` increment) and clones share the same slots.
/// Only this crate hands out [`PacketBuf`] views of a pool's slots; there is
/// no public acquire or receive method.
///
/// # Memory
///
/// Each slot is `buf_len + 1` bytes rounded up to a multiple of 64, so
/// slots never share a cache line. The slab is `slots * stride + 63`
/// bytes, zeroed once at construction; count all of it as committed memory.
/// The free list adds `4 * slots` bytes.
///
/// Slots are reused **without being re-zeroed**. A slot's bytes beyond the
/// length of the packet it currently holds are whatever an earlier packet
/// left there; [`PacketBuf`] never exposes them, as long as the device's
/// `recv` really writes every byte it reports.
///
/// # Example
///
/// ```
/// use tunnel_lattice_async::PacketPool;
///
/// let pool = PacketPool::new(16, 1500)?;
/// assert_eq!(pool.slots(), 16);
/// assert_eq!(pool.buf_len(), 1500);
/// assert_eq!(pool.available(), 16);
///
/// // A zero-length buffer or zero slots is rejected before any allocation.
/// assert!(PacketPool::new(16, 0).is_err());
/// # Ok::<(), tunnel_lattice_async::Error>(())
/// ```
#[derive(Clone)]
pub struct PacketPool {
    inner: Arc<PoolInner>,
}

impl PacketPool {
    /// Upper bound on the slot count [`PacketPool::with_buf_len`] picks.
    pub const DEFAULT_SLOTS: usize = 128;

    /// Slab budget, in bytes, that [`PacketPool::with_buf_len`] divides
    /// into slots (4 MiB).
    pub const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;

    /// Creates a pool of `slots` receive slots of `buf_len` bytes each.
    ///
    /// Any `slots >= 1` and `buf_len >= 1` are accepted as long as the slab
    /// (`slots * stride`, see the type-level docs) fits in `u32::MAX` bytes
    /// and the whole allocation fits in `isize::MAX` bytes. On 64-bit
    /// targets that allows up to 65,472 slots of 65,553 bytes; on 32-bit
    /// targets, 32,736.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidState`] if `slots` or `buf_len` is zero or the sizes
    /// above are exceeded. Nothing is allocated in that case.
    ///
    /// # Aborts
    ///
    /// If the slab allocation itself fails, the process aborts through
    /// [`std::alloc::handle_alloc_error`], as it would for a `Vec`.
    pub fn new(slots: usize, buf_len: usize) -> Result<Self> {
        let (stride, _total, layout) = layout_for(slots, buf_len).ok_or(Error::InvalidState)?;
        Ok(Self::build(
            slots,
            buf_len,
            stride,
            layout,
            #[cfg(test)]
            None,
        ))
    }

    /// Creates a pool of `buf_len`-byte slots with a default slot count:
    /// as many as fit in [`PacketPool::DEFAULT_MAX_BYTES`], clamped to
    /// `1..=`[`PacketPool::DEFAULT_SLOTS`].
    ///
    /// For example `buf_len` 1500 gives 128 slots (192 KiB), 65535 gives
    /// 64 slots and 65553 gives 63 (about 4 MiB each). A `buf_len` whose
    /// slot is larger than the budget gets a single slot.
    ///
    /// # Errors
    ///
    /// As [`PacketPool::new`].
    ///
    /// # Aborts
    ///
    /// As [`PacketPool::new`].
    pub fn with_buf_len(buf_len: usize) -> Result<Self> {
        let stride = stride_for(buf_len).ok_or(Error::InvalidState)?;
        Self::new(default_slots(stride), buf_len)
    }

    fn build(
        slots: usize,
        buf_len: usize,
        stride: usize,
        layout: Layout,
        #[cfg(test)] hooks: Option<Arc<TestHooks>>,
    ) -> Self {
        let slab = Slab::new(layout);
        let mut free = Vec::with_capacity(slots);
        // `slots * stride <= u32::MAX` with `stride >= 64`, so every index
        // fits in `u32`. Reversed so slot 0 is handed out first.
        free.extend((0..slots as u32).rev());
        let inner = Arc::new(PoolInner {
            slab,
            stride,
            buf_len,
            slots,
            state: Mutex::new(FreeState {
                free,
                waiters: Vec::new(),
                spare: Vec::new(),
                live: 0,
                epoch: 0,
                blocked: 0,
                draining: false,
                redrain: false,
            }),
            cv: Condvar::new(),
            #[cfg(test)]
            hooks,
        });
        // Eager initialisation: on platforms whose std mutex and condvar box
        // their pthread object lazily (macOS), pay for it here, once, not on
        // the first packet.
        drop(inner.lock());
        inner.cv.notify_all();
        Self { inner }
    }

    /// Length in bytes of each slot's receive buffer.
    pub fn buf_len(&self) -> usize {
        self.inner.buf_len
    }

    /// Total number of slots.
    pub fn slots(&self) -> usize {
        self.inner.slots
    }

    /// Number of slots free at the moment of the call. Other clones of the
    /// pool may change it immediately afterwards.
    pub fn available(&self) -> usize {
        self.inner.lock().free.len()
    }

    /// An async acquire of one slot: resolves as soon as a slot is free.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the pooled stream adapters")
    )]
    pub(crate) fn acquire(&self) -> Acquire {
        Acquire {
            pool: Some(Arc::clone(&self.inner)),
            entry: None,
        }
    }

    /// Blocks the calling thread until a slot is free or `stop` is set.
    ///
    /// `stop` is checked under the pool mutex, and [`Self::stop_worker`]
    /// sets it under the same mutex before notifying, so a stop request is
    /// never lost.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the pooled stream adapters")
    )]
    pub(crate) fn acquire_blocking(&self, stop: &AtomicBool) -> Option<SlotGuard> {
        let inner = &self.inner;
        let mut st = inner.lock();
        let got = loop {
            if stop.load(Ordering::Acquire) {
                break None;
            }
            if let Some(idx) = st.free.pop() {
                break Some(idx);
            }
            st.blocked += 1;
            st = inner.cv.wait(st).unwrap_or_else(PoisonError::into_inner);
            st.blocked -= 1;
        };
        // Pass a notify on if this thread may have consumed one that another
        // parked thread needs.
        let pass_on = st.blocked > 0 && !st.free.is_empty();
        drop(st);
        if pass_on {
            inner.notify_one();
        }
        got.map(|idx| SlotGuard {
            pool: Some(Arc::clone(inner)),
            idx,
        })
    }

    /// Sets `stop` under the pool mutex and wakes every thread parked in
    /// [`Self::acquire_blocking`]. `notify_all` because other workers on a
    /// shared pool may be parked too, and each re-checks its own flag.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the pooled stream adapters")
    )]
    pub(crate) fn stop_worker(&self, stop: &AtomicBool) {
        let st = self.inner.lock();
        stop.store(true, Ordering::Release);
        drop(st);
        self.inner.cv.notify_all();
    }
}

/// Future returned by [`PacketPool::acquire`]. Owns one pool `Arc`, which
/// it hands to the resulting [`SlotGuard`], and at most one waiter entry.
pub(crate) struct Acquire {
    pool: Option<Arc<PoolInner>>,
    entry: Option<(u64, usize)>,
}

impl Future for Acquire {
    type Output = SlotGuard;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SlotGuard> {
        let this = self.get_mut();
        let pool = this
            .pool
            .as_ref()
            .expect("pool acquire polled after completion");

        // Fast path.
        let mut st = pool.lock();
        let popped = st.free.pop();
        let own = popped.and_then(|_| take_entry(&mut st, &mut this.entry));
        drop(st);
        if let Some(idx) = popped {
            drop(own);
            return Poll::Ready(this.finish(idx));
        }

        // Empty pool: clone before locking, register, retry the pop.
        let waker = cx.waker().clone();
        let mut st = pool.lock();
        if let Some(idx) = st.free.pop() {
            let own = take_entry(&mut st, &mut this.entry);
            drop(st);
            drop(own);
            drop(waker);
            return Poll::Ready(this.finish(idx));
        }
        let displaced = register(&mut st, &mut this.entry, waker);
        drop(st);
        drop(displaced);
        Poll::Pending
    }
}

impl Acquire {
    fn finish(&mut self, idx: u32) -> SlotGuard {
        SlotGuard {
            pool: self.pool.take(),
            idx,
        }
    }
}

impl Drop for Acquire {
    fn drop(&mut self) {
        let Some(pool) = &self.pool else { return };
        if self.entry.is_none() {
            return;
        }
        let waker = take_entry(&mut pool.lock(), &mut self.entry);
        if let Some(waker) = waker {
            drop_guarded(waker);
        }
    }
}

/// Exclusive ownership of one slot between acquire and hand-off. Releases
/// the slot on drop unless converted into a [`PacketBuf`].
pub(crate) struct SlotGuard {
    /// `Some` until [`SlotGuard::into_buf`] moves it into the view.
    pool: Option<Arc<PoolInner>>,
    idx: u32,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by the pooled stream adapters")
)]
impl SlotGuard {
    fn pool(&self) -> &PoolInner {
        self.pool
            .as_deref()
            .expect("a live SlotGuard always owns its pool")
    }

    /// The whole slot: exactly `buf_len` bytes, for `recv` to write into.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        let pool = self.pool();
        let ptr = pool.view_ptr(self.idx as usize * pool.stride);
        let len = pool.buf_len;
        // SAFETY: `idx < slots` (it came off the free list), so
        // `[idx * stride, idx * stride + buf_len)` lies inside the slab
        // (`slots * stride` bytes after `base`) and inside this one slot
        // (`buf_len < stride`). The bytes were zeroed at construction, so
        // they are initialised. This guard is the slot's only owner and
        // `&mut self` prevents a second borrow through it, so the slice is
        // unaliased for its lifetime.
        unsafe { slice::from_raw_parts_mut(ptr, len) }
    }

    /// Converts the guard into a view of the first `n` bytes of its slot.
    ///
    /// Returns `None` if `n > buf_len`; the slot is then released by the
    /// guard's drop.
    pub(crate) fn into_buf(mut self, n: usize) -> Option<PacketBuf> {
        let pool = self.pool();
        if n > pool.buf_len {
            return None;
        }
        let off = self.idx as usize * pool.stride;
        let pool = self.pool.take()?;
        // `off + n < slots * stride <= u32::MAX`, so neither truncates.
        let buf = PacketBuf {
            pool,
            off: off as u32,
            len: n as u32,
        };
        #[cfg(test)]
        buf.assert_contained();
        Some(buf)
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.release(self.idx);
        }
    }
}

/// An exclusive view of one received packet inside a [`PacketPool`] slot.
///
/// Holds the pool alive plus a 32-bit offset and length: 16 bytes on
/// 64-bit targets. Dereferences to `[u8]`; use [`to_vec`](slice::to_vec)
/// for an owned copy. Dropping it returns the slot to the pool, and the
/// slab is freed once the pool and every view of it are gone.
///
/// `PacketBuf` is `Send + Sync`, has no public constructor, and is not
/// `Clone`.
pub struct PacketBuf {
    pool: Arc<PoolInner>,
    off: u32,
    len: u32,
}

impl PacketBuf {
    /// Number of bytes in the view.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the view is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drops the first `n` bytes from the front of the view, for example to
    /// skip a header.
    ///
    /// # Panics
    ///
    /// If `n > self.len()`.
    pub fn advance(&mut self, n: usize) {
        assert!(
            n <= self.len(),
            "cannot advance a PacketBuf past its end: {n} > {}",
            self.len
        );
        // `n <= len`, so both fit in `u32` and `off + len` is unchanged.
        self.off += n as u32;
        self.len -= n as u32;
        #[cfg(test)]
        self.assert_contained();
    }

    /// Shortens the view to its first `len` bytes. Has no effect if `len`
    /// is not less than the current length.
    pub fn truncate(&mut self, len: usize) {
        if len < self.len() {
            // `len < self.len`, so it fits in `u32`.
            self.len = len as u32;
        }
        #[cfg(test)]
        self.assert_contained();
    }

    fn slot(&self) -> u32 {
        // `off < slots * stride <= u32::MAX`, so the quotient fits.
        (self.off as usize / self.pool.stride) as u32
    }

    #[cfg(test)]
    fn assert_contained(&self) {
        let (off, len) = (self.off as usize, self.len as usize);
        let base = self.slot() as usize * self.pool.stride;
        assert!(
            base <= off && off + len <= base + self.pool.buf_len,
            "view [{off}, {}) escapes slot [{base}, {})",
            off + len,
            base + self.pool.buf_len
        );
    }
}

impl Deref for PacketBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let ptr = self.pool.view_ptr(self.off as usize);
        // SAFETY: containment keeps `[off, off + len)` inside this view's
        // slot, which lies inside the slab; the bytes are initialised
        // (zeroed at construction). This view is the slot's only owner, and
        // `&self` excludes a concurrent `&mut` through it.
        unsafe { slice::from_raw_parts(ptr, self.len as usize) }
    }
}

impl DerefMut for PacketBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        let ptr = self.pool.view_ptr(self.off as usize);
        // SAFETY: as in `deref`; `&mut self` makes the borrow unique.
        unsafe { slice::from_raw_parts_mut(ptr, self.len as usize) }
    }
}

impl AsRef<[u8]> for PacketBuf {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl AsMut<[u8]> for PacketBuf {
    fn as_mut(&mut self) -> &mut [u8] {
        self
    }
}

impl fmt::Debug for PacketBuf {
    /// Prints the length only, never the packet bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PacketBuf")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl PartialEq<[u8]> for PacketBuf {
    fn eq(&self, other: &[u8]) -> bool {
        **self == *other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for PacketBuf {
    fn eq(&self, other: &[u8; N]) -> bool {
        **self == other[..]
    }
}

impl Drop for PacketBuf {
    fn drop(&mut self) {
        self.pool.release(self.slot());
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<PacketBuf>() == 16);
    assert!(size_of::<Option<PacketBuf>>() == 16);
    assert!(size_of::<Result<PacketBuf>>() == 16);
};

#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<PacketBuf>() == 12);

const _: fn() = || {
    fn send_unpin<T: Send + Unpin + 'static>() {}
    fn send_sync<T: Send + Sync + 'static>() {}
    send_unpin::<crate::PacketStream>();
    send_sync::<PacketBuf>();
    send_sync::<PacketPool>();
};

/// Per-pool test hooks.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    /// Number of `notify_one` calls made by release and by
    /// `acquire_blocking`'s pass-on.
    notify_count: std::sync::atomic::AtomicUsize,
    /// Runs once, in the slow path of release, after the drained wakers
    /// were woken and before the pool mutex is locked again.
    release_between_unlock_and_relock: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Set when `PoolInner` (and so the slab) is dropped.
    inner_dropped: AtomicBool,
}

#[cfg(test)]
impl PacketPool {
    fn with_hooks(slots: usize, buf_len: usize, hooks: Arc<TestHooks>) -> Result<Self> {
        let (stride, _total, layout) = layout_for(slots, buf_len).ok_or(Error::InvalidState)?;
        Ok(Self::build(slots, buf_len, stride, layout, Some(hooks)))
    }

    fn stride(&self) -> usize {
        self.inner.stride
    }

    fn base_addr(&self) -> usize {
        self.inner.slab.base.as_ptr().addr()
    }

    fn free_top(&self) -> Option<u32> {
        self.inner.lock().free.last().copied()
    }

    fn waiters_len(&self) -> usize {
        self.inner.lock().waiters.len()
    }

    fn live(&self) -> usize {
        self.inner.lock().live
    }

    fn blocked(&self) -> u32 {
        self.inner.lock().blocked
    }

    fn set_blocked(&self, blocked: u32) {
        self.inner.lock().blocked = blocked;
    }

    /// `(waiters ptr, waiters capacity, spare ptr, spare capacity)`.
    fn buffers(&self) -> (usize, usize, usize, usize) {
        let st = self.inner.lock();
        (
            st.waiters.as_ptr().addr(),
            st.waiters.capacity(),
            st.spare.as_ptr().addr(),
            st.spare.capacity(),
        )
    }

    fn is_poisoned(&self) -> bool {
        self.inner.state.is_poisoned()
    }

    fn poison(&self) {
        let inner = Arc::clone(&self.inner);
        let result = catch_unwind(AssertUnwindSafe(move || {
            let _guard = inner.state.lock();
            panic!("poisoning the pool mutex on purpose");
        }));
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    const DEADLINE: Duration = Duration::from_secs(60);

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn take(pool: &PacketPool) -> SlotGuard {
        pool.acquire_blocking(&never()).expect("stop is never set")
    }

    fn take_buf(pool: &PacketPool, n: usize) -> PacketBuf {
        take(pool).into_buf(n).expect("n <= buf_len")
    }

    fn poll(fut: &mut Acquire, waker: &Waker) -> Poll<SlotGuard> {
        Pin::new(fut).poll(&mut Context::from_waker(waker))
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < DEADLINE, "timed out waiting for {what}");
            thread::yield_now();
        }
    }

    /// Counts wakes.
    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Counter {
        fn waker() -> (Arc<Self>, Waker) {
            let counter = Arc::new(Self::default());
            (Arc::clone(&counter), Waker::from(counter))
        }

        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Panics when woken.
    struct PanicOnWake;

    impl Wake for PanicOnWake {
        fn wake(self: Arc<Self>) {
            panic!("test waker panics on wake");
        }
    }

    /// Panics when its last clone is dropped. Counts wakes, so it is not a
    /// no-op waker (`Waker::noop()` cannot panic on drop).
    struct PanicOnDrop(AtomicUsize);

    impl Wake for PanicOnDrop {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("test waker panics on drop");
        }
    }

    // Test 7: construction limits.

    #[test]
    fn pool_rejects_degenerate_and_oversized_layouts() {
        assert!(layout_for(0, 1500).is_none());
        assert!(layout_for(1, 0).is_none());
        assert!(layout_for(1, usize::MAX).is_none());
        assert!(layout_for(usize::MAX, 1).is_none());
        // Over the `u32` offset bound.
        assert!(layout_for(1, u32::MAX as usize).is_none());
        for (slots, buf_len) in [(0, 1500), (1, 0), (usize::MAX, 1500), (2, usize::MAX)] {
            assert!(matches!(
                PacketPool::new(slots, buf_len),
                Err(Error::InvalidState)
            ));
        }
        assert!(matches!(
            PacketPool::with_buf_len(0),
            Err(Error::InvalidState)
        ));
        assert!(matches!(
            PacketPool::with_buf_len(usize::MAX),
            Err(Error::InvalidState)
        ));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn pool_layout_e5_pins_64bit() {
        assert!(matches!(
            PacketPool::new(1 << 58, 1),
            Err(Error::InvalidState)
        ));
        assert!(layout_for(65_472, 65_553).is_some());
        assert!(layout_for(65_473, 65_553).is_none());
        assert!(layout_for(65_535, 65_535).is_some());
        assert!(layout_for(65_536, 65_535).is_none());
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn pool_layout_e5_pins_32bit() {
        assert!(matches!(
            PacketPool::new(1 << 26, 1),
            Err(Error::InvalidState)
        ));
        assert!(layout_for(32_736, 65_553).is_some());
        assert!(layout_for(32_737, 65_553).is_none());
        assert!(layout_for(32_767, 65_535).is_some());
        assert!(layout_for(32_768, 65_535).is_none());
    }

    #[test]
    fn pool_stride_and_default_slots() {
        for (buf_len, stride) in [
            (1, 64),
            (63, 64),
            (64, 128),
            (1500, 1536),
            (1518, 1536),
            (9000, 9024),
            (9018, 9024),
            (65_535, 65_536),
            (65_553, 65_600),
        ] {
            assert_eq!(stride_for(buf_len), Some(stride), "buf_len {buf_len}");
        }
        for (buf_len, slots) in [
            (1500, 128),
            (1518, 128),
            (9018, 128),
            (65_535, 64),
            (65_553, 63),
            (PacketPool::DEFAULT_MAX_BYTES, 1),
        ] {
            assert_eq!(
                default_slots(stride_for(buf_len).unwrap()),
                slots,
                "buf_len {buf_len}"
            );
        }
        let big = PacketPool::new(1, 65_553).unwrap();
        assert_eq!(
            (big.stride(), big.slots(), big.buf_len()),
            (65_600, 1, 65_553)
        );
        let tun = PacketPool::new(1, 65_535).unwrap();
        assert_eq!(tun.stride(), 65_536);
        let default = PacketPool::with_buf_len(1500).unwrap();
        assert_eq!((default.slots(), default.available()), (128, 128));
    }

    #[test]
    fn pool_slab_is_aligned_and_zeroed() {
        for buf_len in [1, 64, 100, 1500] {
            let pool = PacketPool::new(3, buf_len).unwrap();
            assert_eq!(pool.base_addr() % SLOT_ALIGN, 0);
            assert_eq!(pool.stride() % SLOT_ALIGN, 0);
            let mut guards: Vec<SlotGuard> = (0..3).map(|_| take(&pool)).collect();
            for guard in &mut guards {
                let slot = guard.as_mut_slice();
                assert_eq!(slot.len(), buf_len);
                assert_eq!(slot.as_ptr().addr() % SLOT_ALIGN, 0);
                assert!(slot.iter().all(|&b| b == 0));
            }
        }
    }

    // Test 6: late drop.

    #[test]
    fn pool_slab_freed_when_last_packet_buf_drops() {
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(2, 64, Arc::clone(&hooks)).unwrap();
        let mut guard = take(&pool);
        guard.as_mut_slice()[..3].copy_from_slice(b"abc");
        let buf = guard.into_buf(3).unwrap();
        drop(pool);
        assert!(!hooks.inner_dropped.load(Ordering::SeqCst));
        assert_eq!(buf, *b"abc");
        drop(buf);
        assert!(hooks.inner_dropped.load(Ordering::SeqCst));
    }

    // Test 8: E2/E3 containment.

    #[test]
    fn pool_advance_full_view_releases_the_right_slot() {
        for buf_len in [64, 1536, 9024] {
            let pool = PacketPool::new(3, buf_len).unwrap();
            let first = take(&pool);
            let mut second = take(&pool).into_buf(buf_len).unwrap();
            assert_eq!(second.slot(), 1);
            second.advance(buf_len);
            assert!(second.is_empty());
            assert_eq!(second.slot(), 1, "advance(len) must stay in its slot");
            drop(second);
            assert_eq!(pool.free_top(), Some(1));
            drop(first);
            assert_eq!(pool.free_top(), Some(0));
            assert_eq!(pool.available(), 3);
        }
    }

    #[test]
    fn pool_disjoint_views_stay_readable_and_writable() {
        let pool = PacketPool::new(2, 64).unwrap();
        let mut a = take_buf(&pool, 64);
        let mut b = take_buf(&pool, 64);
        a.fill(0xAA);
        b.fill(0xBB);
        a.advance(10);
        a.truncate(20);
        a.fill(0xCC);
        assert_eq!(a.len(), 20);
        assert!(a.iter().all(|&x| x == 0xCC));
        assert!(b.iter().all(|&x| x == 0xBB));
        b[63] = 1;
        assert_eq!(b[63], 1);
        drop(a);
        drop(b);
        assert_eq!(pool.available(), 2);
    }

    // Test 9: PacketBuf API.

    #[test]
    fn packet_buf_api() {
        let pool = PacketPool::new(1, 16).unwrap();
        let mut guard = take(&pool);
        guard.as_mut_slice()[..5].copy_from_slice(b"hello");
        let mut buf = guard.into_buf(5).unwrap();
        assert_eq!(buf, *b"hello");
        assert_eq!(buf, b"hello"[..]);
        assert_eq!(&*buf, b"hello");
        assert_eq!(buf.as_ref(), b"hello");
        assert_eq!(buf.len(), 5);
        assert!(!buf.is_empty());
        assert_eq!(format!("{buf:?}"), "PacketBuf { len: 5, .. }");

        buf.truncate(10);
        assert_eq!(buf.len(), 5, "truncate to a larger length is a no-op");
        buf.truncate(4);
        assert_eq!(buf, *b"hell");
        buf.advance(1);
        assert_eq!(buf, *b"ell");
        buf.as_mut()[0] = b'E';
        assert_eq!(buf.to_vec(), b"Ell".to_vec());
        buf.advance(3);
        assert!(buf.is_empty());
        assert_eq!(buf, []);
    }

    #[test]
    #[should_panic(expected = "cannot advance a PacketBuf past its end")]
    fn packet_buf_advance_past_end_panics() {
        let pool = PacketPool::new(1, 16).unwrap();
        let mut buf = take_buf(&pool, 4);
        buf.advance(5);
    }

    // Test 10: poison tolerance.

    #[test]
    fn pool_tolerates_a_poisoned_mutex() {
        let pool = PacketPool::new(1, 16).unwrap();
        let buf = take_buf(&pool, 4);
        pool.poison();
        assert!(pool.is_poisoned());
        assert_eq!(pool.available(), 0);
        drop(buf);
        assert_eq!(pool.available(), 1);

        // A release while unwinding does not panic again (which would abort).
        let buf = take_buf(&pool, 4);
        let result = catch_unwind(AssertUnwindSafe(move || {
            let _buf = buf;
            panic!("unwinding with a PacketBuf alive");
        }));
        assert!(result.is_err());
        assert_eq!(pool.available(), 1);
    }

    // Test 11: handoff releases each slot exactly once.

    #[test]
    fn pool_handoff_releases_each_slot_once() {
        let pool = PacketPool::new(2, 16).unwrap();

        let buf = take(&pool).into_buf(16).unwrap();
        assert_eq!(pool.available(), 1);
        drop(buf);
        assert_eq!(pool.available(), 2);

        drop(take(&pool));
        assert_eq!(pool.available(), 2);

        let guard = take(&pool);
        assert_eq!(pool.available(), 1);
        assert!(guard.into_buf(17).is_none());
        assert_eq!(pool.available(), 2);

        let empty = take(&pool).into_buf(0).unwrap();
        assert!(empty.is_empty());
        drop(empty);
        assert_eq!(pool.available(), 2);
        // `release` asserts in test builds that an index is never pushed
        // twice, so reaching here also proves no double release happened.
    }

    // Test 12: notify gating.

    #[test]
    fn pool_notifies_only_when_a_worker_is_parked() {
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(1, 16, Arc::clone(&hooks)).unwrap();
        for _ in 0..3 {
            drop(take_buf(&pool, 1));
        }
        assert_eq!(hooks.notify_count.load(Ordering::SeqCst), 0);

        let held = take_buf(&pool, 1);
        let worker_pool = pool.clone();
        let worker = thread::spawn(move || worker_pool.acquire_blocking(&never()).is_some());
        wait_until("the worker to park", || pool.blocked() == 1);
        drop(held);
        assert!(worker.join().unwrap());
        assert_eq!(hooks.notify_count.load(Ordering::SeqCst), 1);
        assert_eq!(pool.available(), 1);
    }

    // r6-3: stop under the mutex plus notify_all, on a shared pool.

    #[test]
    fn pool_stop_worker_exits_only_the_stopped_worker() {
        let pool = PacketPool::new(1, 16).unwrap();
        let already = AtomicBool::new(true);
        assert!(pool.acquire_blocking(&already).is_none());

        let held = take_buf(&pool, 1);
        let stop_a = Arc::new(never());
        let stop_b = Arc::new(never());
        let spawn = |stop: &Arc<AtomicBool>| {
            let (pool, stop) = (pool.clone(), Arc::clone(stop));
            thread::spawn(move || pool.acquire_blocking(&stop).is_some())
        };
        let a = spawn(&stop_a);
        let b = spawn(&stop_b);
        wait_until("both workers to park", || pool.blocked() == 2);

        pool.stop_worker(&stop_a);
        assert!(!a.join().unwrap(), "stopped worker exits without a slot");
        wait_until("the surviving worker to re-park", || pool.blocked() == 1);

        drop(held);
        assert!(b.join().unwrap(), "surviving worker gets the released slot");
        assert_eq!(pool.available(), 1);
    }

    // Test 13: waker recycling with an overlapping release (also r6-4).

    #[test]
    fn pool_overlapping_release_loses_no_waker_and_recycles_buffers() {
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(2, 16, Arc::clone(&hooks)).unwrap();

        // Warm both waiter buffers up: two exhaust/register/release cycles.
        for _ in 0..2 {
            let a = take_buf(&pool, 1);
            let b = take_buf(&pool, 1);
            let (_, waker) = Counter::waker();
            let mut fut = pool.acquire();
            assert!(poll(&mut fut, &waker).is_pending());
            drop(a);
            assert!(poll(&mut fut, &waker).is_ready());
            drop(b);
        }
        let (wp, wc, sp, sc) = pool.buffers();
        assert!(wc > 0 && sc > 0 && wp != sp);

        let g1 = take(&pool);
        let g2 = take(&pool);
        let (c1, w1) = Counter::waker();
        let (c2, w2) = Counter::waker();
        let mut f1 = pool.acquire();
        assert!(poll(&mut f1, &w1).is_pending());

        type Stash = Arc<Mutex<Vec<(Acquire, SlotGuard)>>>;
        let stash: Stash = Arc::default();
        let hook_stash = Arc::clone(&stash);
        let hook_pool = pool.clone();
        let hook_c1 = Arc::clone(&c1);
        *hooks.release_between_unlock_and_relock.lock().unwrap() = Some(Box::new(move || {
            // Release A woke `w1`; `f1` now takes g1's slot.
            assert_eq!(hook_c1.count(), 1);
            let got = match poll(&mut f1, &w1) {
                Poll::Ready(guard) => guard,
                Poll::Pending => panic!("f1 was woken with a free slot"),
            };
            // A new waiter registers in the swapped-in buffer...
            let mut f2 = hook_pool.acquire();
            assert!(poll(&mut f2, &w2).is_pending());
            // ...and release B overlaps release A's slow path.
            drop(g2);
            hook_stash.lock().unwrap().push((f2, got));
        }));

        drop(g1);

        assert_eq!(c1.count(), 1);
        assert_eq!(
            c2.count(),
            1,
            "the waiter registered during overlap is woken"
        );
        let (mut f2, got) = stash.lock().unwrap().pop().unwrap();
        let (_, noop) = Counter::waker();
        assert!(poll(&mut f2, &noop).is_ready());
        drop(got);
        assert_eq!(pool.available(), 2);
        assert_eq!(pool.live(), 0);

        let (wp2, wc2, sp2, sc2) = pool.buffers();
        let mut before = [(wp, wc), (sp, sc)];
        let mut after = [(wp2, wc2), (sp2, sc2)];
        before.sort_unstable();
        after.sort_unstable();
        assert_eq!(before, after, "release must only swap the two buffers");
    }

    // r6-5: tombstones alone never take the slow path.

    #[test]
    fn pool_tombstones_are_compacted_without_a_drain() {
        let pool = PacketPool::new(1, 16).unwrap();
        let held = take_buf(&pool, 1);
        let (count, waker) = Counter::waker();
        let mut fut = pool.acquire();
        assert!(poll(&mut fut, &waker).is_pending());
        drop(fut);
        assert_eq!((pool.waiters_len(), pool.live()), (1, 0));
        let (wp, wc, sp, sc) = pool.buffers();
        drop(held);
        assert_eq!(pool.waiters_len(), 0);
        assert_eq!(
            pool.buffers(),
            (wp, wc, sp, sc),
            "compaction is in place, not a swap"
        );
        assert_eq!(count.count(), 0);
    }

    // Test 15: a panicking drained waker.

    #[test]
    fn pool_panicking_drained_waker_is_contained() {
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(1, 16, Arc::clone(&hooks)).unwrap();
        let held = take_buf(&pool, 1);

        let (ca, wa) = Counter::waker();
        let wb = Waker::from(Arc::new(PanicOnWake));
        let (cc, wc) = Counter::waker();
        let mut futs = [pool.acquire(), pool.acquire(), pool.acquire()];
        for (fut, waker) in futs.iter_mut().zip([&wa, &wb, &wc]) {
            assert!(poll(fut, waker).is_pending());
        }
        assert_eq!(pool.live(), 3);
        // Stands in for a bridge worker parked in `acquire_blocking` (the
        // real parked-thread path is covered by the notify-gating test);
        // keeps `available()` deterministic.
        pool.set_blocked(1);

        drop(held);

        assert_eq!(hooks.notify_count.load(Ordering::SeqCst), 1);
        assert_eq!((ca.count(), cc.count()), (1, 1));
        assert_eq!(pool.available(), 1);
        assert_eq!((pool.waiters_len(), pool.live()), (0, 0));
        let (_, wcap, _, scap) = pool.buffers();
        assert!(scap >= 3, "the drained buffer is kept as spare");
        assert_eq!(wcap, 0);
        pool.set_blocked(0);
        drop(futs);
    }

    #[test]
    fn pool_panicking_drained_waker_during_unwind_does_not_abort() {
        let pool = PacketPool::new(1, 16).unwrap();
        let held = take_buf(&pool, 1);
        let wb = Waker::from(Arc::new(PanicOnWake));
        let mut fut = pool.acquire();
        assert!(poll(&mut fut, &wb).is_pending());

        let result = catch_unwind(AssertUnwindSafe(move || {
            let _held = held;
            panic!("unwinding while releasing to a panicking waiter");
        }));
        assert!(result.is_err());
        assert_eq!(pool.available(), 1);
        drop(fut);
    }

    // Test 17: E14 auto traits (the const assertions above are the pin).

    #[test]
    fn pool_types_have_the_promised_auto_traits() {
        fn clone_send_sync<T: Clone + Send + Sync + 'static>() {}
        clone_send_sync::<PacketPool>();
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<Result<PacketBuf>>(), 16);
    }

    // Test 18: waiter bound.

    #[test]
    fn pool_one_acquire_holds_at_most_one_waiter_entry() {
        let pool = PacketPool::new(1, 16).unwrap();
        let held = take_buf(&pool, 1);
        let mut fut = pool.acquire();
        let mut counters = Vec::new();
        for _ in 0..100 {
            let (count, waker) = Counter::waker();
            assert!(poll(&mut fut, &waker).is_pending());
            assert!(pool.waiters_len() <= 1);
            assert_eq!(pool.live(), 1);
            counters.push(count);
        }
        // The same waker again is kept, not re-stored.
        let (_, same) = Counter::waker();
        assert!(poll(&mut fut, &same).is_pending());
        assert!(poll(&mut fut, &same).is_pending());
        assert_eq!(pool.waiters_len(), 1);

        drop(fut);
        assert_eq!(pool.live(), 0);

        // A pending acquire whose stored waker panics on drop is dropped
        // without the panic escaping.
        let mut fut = pool.acquire();
        let waker = Waker::from(Arc::new(PanicOnDrop(AtomicUsize::new(0))));
        assert!(poll(&mut fut, &waker).is_pending());
        drop(waker);
        drop(fut);
        assert_eq!(pool.live(), 0);

        drop(held);
        assert_eq!(pool.available(), 1);
        assert!(counters.iter().all(|c| c.count() == 0));
    }

    // Multi-thread acquire/release contention (blocking and async mixed).

    #[test]
    fn pool_contended_acquire_release() {
        const THREADS: usize = 6;
        let rounds = if cfg!(miri) { 12 } else { 2_000 };
        let pool = PacketPool::new(2, 64).unwrap();
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let pool = pool.clone();
                thread::spawn(move || {
                    let tag = t as u8 + 1;
                    for round in 0..rounds {
                        let mut guard = if (t + round) % 2 == 0 {
                            take(&pool)
                        } else {
                            futures::executor::block_on(pool.acquire())
                        };
                        guard.as_mut_slice()[..32].fill(tag);
                        let mut buf = guard.into_buf(32).unwrap();
                        thread::yield_now();
                        assert!(buf.iter().all(|&b| b == tag), "slot shared by two owners");
                        buf.advance(8);
                        buf.truncate(8);
                        assert_eq!(buf, [tag; 8]);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(pool.available(), 2);
        assert_eq!((pool.blocked(), pool.live()), (0, 0));
    }
}
