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
//! 5. Dereferencing a [`PoolRef`] while it holds a counted reference.
//! 6. `Box::from_raw` of the pool state after the reference count's final
//!    decrement and an `Acquire` fence.
//! 7. `unsafe impl Send/Sync for PoolRef`.
//! 8. Minting a [`PoolRef`] from an [`Acquirer`]'s prepaid credit
//!    ([`PoolRef::credit_ref`]): the caller gives up one unit of credit.
//! 9. The plain-store slot claim of an exclusive [`Acquirer`], which relies
//!    on it being the only acquirer of its pool (see [`Acquirer::new`]).
//!    This is atomic code, but a second claimer would hand one slot to two
//!    owners, so it is listed here.
//!
//! **Ownership invariant.** Each slot has one atomic flag, `FREE` or
//! `OWNED`. From the moment an acquirer claims a slot (`FREE` to `OWNED`)
//! until its owner stores `FREE` again, exactly one [`SlotGuard`] or one
//! [`PacketBuf`] owns it. Only an acquirer claims, and only the owner
//! releases. The handoff between the two owners is safe code
//! (`Option::take` of the pool reference).
//!
//! **Reference count.** The pool state is freed when its count reaches
//! zero. The count covers every [`PacketPool`] handle, every live
//! [`SlotGuard`] and [`PacketBuf`], and every acquirer's unspent prepaid
//! credit. Its top bit, `WAITING`, says that a waiter may be registered;
//! a waiter sets it under the pool mutex after registering and then scans
//! the flags again, and a release that sees it takes the locked slow path.
//!
//! **Invariant P1.** No thread touches the pool state after the atomic
//! operation that gives up its counted reference, except to free the state
//! when that operation took the count to zero. That is why a release that
//! sees `WAITING` first clears the bit while it still holds its reference,
//! runs the slow path, and only then decrements.
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
#[cfg(test)]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering, fence};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use tunnel_lattice_core::{Error, Result};

/// Slot and slab alignment: one cache line.
const SLOT_ALIGN: usize = 64;

/// Slot flag: free to claim. Non-zero on purpose, so the flags are a plain
/// (not zeroed) allocation.
const FREE: u8 = 1;
/// Slot flag: owned by one `SlotGuard` or `PacketBuf`.
const OWNED: u8 = 0;

/// Top bit of `PoolInner::refs`: a waiter may be registered.
const WAITING: usize = 1 << (usize::BITS - 1);
/// The reference-count bits of `PoolInner::refs`.
const COUNT: usize = !WAITING;
/// Above this count the process aborts, as `Arc` does near `isize::MAX`.
const MAX_REFS: usize = COUNT / 2;
/// References an [`Acquirer`] prepays per refill.
const CREDIT_BATCH: usize = 64;

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

/// `ceil(2^64 / stride)`, the multiplier [`div_stride`] divides by.
///
/// `stride >= 64`, so the addition cannot overflow.
fn stride_recip(stride: usize) -> u64 {
    u64::MAX / stride as u64 + 1
}

/// `off / stride` without a hardware division, given `recip =
/// stride_recip(stride)`; `PacketBuf::drop` runs this once per packet.
///
/// Exact for every `u32` numerator and every divisor below `2^32` (Lemire,
/// Kaser and Kurz, "Faster Remainder by Direct Computation", 2019); a
/// pool's `stride` is at most `u32::MAX` because its slab is.
#[inline]
fn div_stride(off: u32, recip: u64) -> u32 {
    ((u128::from(recip) * u128::from(off)) >> 64) as u32
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
// exclusivity is guaranteed by the slot flags (one owner per claimed slot),
// so sharing the pointer between threads cannot create a data race.
unsafe impl Send for Slab {}
// SAFETY: `&Slab` exposes no access to the bytes by itself; see `Send`.
unsafe impl Sync for Slab {}

/// Waiter state, guarded by `PoolInner::state`. Only the slow paths (a
/// waiting acquirer, a release that sees `WAITING`) lock it.
///
/// Every change under the lock is a step that cannot stop half-way: a push
/// within reserved capacity, a counter step, an entry replace or take, or a
/// `mem::replace`/`mem::take`.
struct FreeState {
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

/// Shared pool state. Freed (slab included) when its reference count
/// reaches zero: once the last `PacketPool`, `PacketBuf`, `SlotGuard`, and
/// acquirer are gone.
pub(crate) struct PoolInner {
    slab: Slab,
    stride: usize,
    /// [`stride_recip`]`(stride)`, for [`div_stride`].
    stride_recip: u64,
    buf_len: usize,
    slots: usize,
    /// One ownership flag per slot, [`FREE`] or [`OWNED`].
    flags: Box<[AtomicU8]>,
    /// The slot released most recently: only a hint for [`scan`], which
    /// tries it right after the acquirer's own cursor. Always `< slots`.
    last_freed: AtomicU32,
    /// Reference count in the [`COUNT`] bits, plus the [`WAITING`] bit.
    refs: AtomicUsize,
    /// Number of live `PacketPool` handles; read only by [`Acquirer::new`].
    handles: AtomicUsize,
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
    #[inline]
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

    /// Marks slot `idx` free. Called only by the slot's single owner, right
    /// before it drops its counted reference (whose decrement then sees
    /// `WAITING` and wakes waiters, if any).
    #[inline]
    fn release(&self, idx: u32) {
        let flag = &self.flags[idx as usize];
        #[cfg(test)]
        assert!(
            flag.load(Ordering::Relaxed) == OWNED,
            "slot {idx} released twice"
        );
        // `Release`: the owner's writes to the slot happen-before the next
        // claim (`Acquire`), and before a waiter's rescan that follows the
        // owner's decrement in `refs`' modification order.
        flag.store(FREE, Ordering::Release);
        // A plain store: the hint orders nothing, `scan` still claims
        // through the flag.
        self.last_freed.store(idx, Ordering::Relaxed);
    }

    /// Increments the reference count by `n`. Aborts on overflow.
    fn add_refs(&self, n: usize) {
        let old = self.refs.fetch_add(n, Ordering::Relaxed);
        if old & COUNT > MAX_REFS {
            std::process::abort();
        }
    }

    /// Sets `WAITING`. Called under the pool mutex, after registering.
    fn arm_waiting(&self) {
        self.refs.fetch_or(WAITING, Ordering::AcqRel);
    }

    /// Scans for a free flag without claiming it.
    fn any_free(&self) -> bool {
        self.flags
            .iter()
            .any(|flag| flag.load(Ordering::Relaxed) == FREE)
    }

    /// Slow path of a release that saw `WAITING`: wakes whoever waits for a
    /// slot. The caller still holds its counted reference (P1).
    #[cold]
    #[inline(never)]
    fn wake_waiters(&self) {
        let mut st = self.lock();
        let drained = if st.live > 0 {
            if st.draining {
                st.redrain = true;
                None
            } else {
                #[cfg(test)]
                if let Some(hooks) = &self.hooks {
                    hooks.drain_count.fetch_add(1, Ordering::SeqCst);
                }
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
        if need_notify {
            // Re-arm, so every later release keeps notifying while a worker
            // is parked (one notify wakes only one of several workers).
            self.arm_waiting();
        }
        drop(st);

        if need_notify {
            self.notify_one();
        }
        if let Some(drained) = drained {
            self.finish_drain(drained);
        }
    }

    /// Second half of [`Self::wake_waiters`]: wakes a drained buffer outside the
    /// lock, re-drains while overlapping releases asked for it, then puts
    /// the (now empty) buffer back as `spare`. Only two waiter buffers ever
    /// exist, so this never allocates.
    ///
    /// The re-drain loop has no fixed bound: a release that keeps
    /// overlapping with new waiters on a shared pool can keep one drainer
    /// looping. This is accepted, not bounded. Each extra pass runs only
    /// because another release freed a slot *and* a live waiter registered
    /// meanwhile, and it wakes that waiter, so every pass is progress for
    /// some consumer; the drainer's extra work is proportional to the
    /// concurrent activity of other threads, never a spin. Stopping early
    /// instead would strand live waiters while slots are free (a lost
    /// wake-up), because no later release is guaranteed to come.
    fn finish_drain(&self, mut drained: Vec<Option<Waker>>) {
        // Clears `draining` if a panic escapes `wake_all` (only possible
        // when a caught panic payload's own `Drop` panics), so later async
        // waiters are still woken. Disarmed on the normal return.
        let reset = DrainReset(self);
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
            drop(st);
            mem::forget(reset);
            return;
        }
    }

    #[cfg(test)]
    fn hook_release_between_unlock_and_relock(&self) {
        if let Some(hooks) = &self.hooks {
            run_hook(&hooks.release_between_unlock_and_relock);
        }
    }
}

#[cfg(test)]
impl Drop for PoolInner {
    fn drop(&mut self) {
        if let Some(hooks) = &self.hooks {
            hooks.inner_dropped.store(true, Ordering::SeqCst);
            hooks.inner_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
}

const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    // `PoolRef`'s `Send`/`Sync` impls rely on this.
    send_sync::<PoolInner>();
};

/// One counted reference to a heap-allocated [`PoolInner`]: the pool's
/// own `Arc`, with a [`WAITING`] bit in the count and a release that keeps
/// its reference while it wakes waiters (invariant P1).
pub(crate) struct PoolRef(NonNull<PoolInner>);

// SAFETY: a `PoolRef` is a counted shared reference to a `PoolInner`, which
// is `Send + Sync` (asserted above), exactly like `Arc<PoolInner>`; the
// count itself is atomic.
unsafe impl Send for PoolRef {}
// SAFETY: as for `Send`; `&PoolRef` only gives `&PoolInner`.
unsafe impl Sync for PoolRef {}

impl PoolRef {
    /// Allocates `inner` with a count of one, owned by the returned value.
    fn new(inner: PoolInner) -> Self {
        Self(NonNull::from(Box::leak(Box::new(inner))))
    }

    /// Another counted reference. Aborts on count overflow.
    fn clone_ref(&self) -> Self {
        self.add_refs(1);
        Self(self.0)
    }

    /// A reference paid for in advance by [`PoolInner::add_refs`].
    ///
    /// # Safety
    ///
    /// The caller must hold one unit of prepaid credit on this pool (a count
    /// it added and has not given to another `PoolRef` or returned), and
    /// gives it up to the returned value.
    #[inline]
    unsafe fn credit_ref(&self) -> Self {
        Self(self.0)
    }

    /// Gives up one counted reference of `ptr` and frees the pool state if
    /// it was the last. Touches nothing afterwards (P1).
    ///
    /// # Safety
    ///
    /// The caller holds one counted reference to `ptr` and never uses it
    /// again.
    unsafe fn release_ref(ptr: NonNull<PoolInner>) {
        // SAFETY: the caller's counted reference keeps `*ptr` alive for
        // this access.
        let old = unsafe { ptr.as_ref() }.refs.fetch_sub(1, Ordering::Release);
        if old & COUNT == 1 {
            // SAFETY: as in `PoolRef::drop`.
            unsafe { Self::free(ptr) }
        }
    }

    /// Frees the pool state.
    ///
    /// # Safety
    ///
    /// The caller's decrement took the count from one to zero, so no other
    /// reference exists and nothing can touch `*ptr` again.
    unsafe fn free(ptr: NonNull<PoolInner>) {
        // Every other decrement was `Release`; this makes all their accesses
        // to the state happen-before the drop.
        fence(Ordering::Acquire);
        // SAFETY: `ptr` came from `Box::leak` in `PoolRef::new` and, per the
        // caller's precondition, this is the only and last use of it.
        drop(unsafe { Box::from_raw(ptr.as_ptr()) });
    }

    /// Slow path of the drop: `WAITING` was cleared by this thread while it
    /// still holds its reference. Wakes waiters, then gives the reference
    /// up, also if a waker's panic escapes the wake.
    #[cold]
    #[inline(never)]
    fn drop_slow(&self) {
        struct GiveUp(NonNull<PoolInner>);
        impl Drop for GiveUp {
            fn drop(&mut self) {
                // SAFETY: this guard owns the dropping `PoolRef`'s counted
                // reference, which nothing uses after this.
                unsafe { PoolRef::release_ref(self.0) }
            }
        }
        let give_up = GiveUp(self.0);
        self.wake_waiters();
        drop(give_up);
    }
}

impl Deref for PoolRef {
    type Target = PoolInner;

    #[inline]
    fn deref(&self) -> &PoolInner {
        // SAFETY: this `PoolRef` holds a counted reference, so the
        // allocation from `PoolRef::new` is alive and is only freed after
        // the count reaches zero; it is only ever accessed through `&`.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for PoolRef {
    #[inline]
    fn drop(&mut self) {
        let refs = &self.refs;
        let mut cur = refs.load(Ordering::Relaxed);
        loop {
            if cur & WAITING == 0 {
                // `Release`: everything this reference did (including a slot
                // flag store) happens-before the free, and before a waiter
                // whose `fetch_or` follows this in modification order.
                match refs.compare_exchange_weak(cur, cur - 1, Ordering::Release, Ordering::Relaxed)
                {
                    Ok(_) => {
                        if cur == 1 {
                            // SAFETY: the count went from one to zero.
                            unsafe { Self::free(self.0) }
                        }
                        return;
                    }
                    Err(actual) => cur = actual,
                }
            } else {
                // Clear `WAITING` while keeping this reference (P1).
                match refs.compare_exchange_weak(
                    cur,
                    cur & !WAITING,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return self.drop_slow(),
                    Err(actual) => cur = actual,
                }
            }
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

/// Unwind guard of [`PoolInner::finish_drain`]. Its `Drop` runs only if a
/// panic escaped the drain (the normal path `mem::forget`s it): it clears
/// `draining` and `redrain` so the next release drains again, and wakes any
/// live waiter that registered while the drain was running, since the
/// interrupted drainer can no longer do it.
struct DrainReset<'a>(&'a PoolInner);

impl Drop for DrainReset<'_> {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.draining = false;
        st.redrain = false;
        // Taking `waiters` leaves an empty, unallocated `Vec`; nothing
        // foreign runs under the lock.
        let stranded = (st.live > 0).then(|| {
            st.epoch = st.epoch.wrapping_add(1);
            st.live = 0;
            mem::take(&mut st.waiters)
        });
        drop(st);
        if let Some(mut stranded) = stranded {
            wake_all(&mut stranded);
        }
    }
}

/// Drops a foreign waker outside the consumer's poll without letting a
/// panic escape.
pub(crate) fn drop_guarded(waker: Waker) {
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
/// Cloning is cheap (two atomic increments, no allocation) and clones share
/// the same slots.
/// Only this crate hands out [`PacketBuf`] views of a pool's slots; there is
/// no public acquire or receive method. A [`PacketStream`](crate::PacketStream)
/// receives every packet into one of its pool's slots:
/// [`from_async_device`](crate::from_async_device) and
/// [`from_device`](crate::from_device) build a pool with
/// [`PacketPool::with_buf_len`], and the `*_with_pool` constructors take one
/// you built.
///
/// # Back-pressure and sharing
///
/// A stream receives only while its pool has a free slot. Items the
/// consumer still holds, and items queued inside the stream, keep their
/// slots; once every slot is taken the stream stops calling `recv` and
/// waits (it never returns an error for this) until an item is dropped.
/// Several streams may share one pool through clones; they then share its
/// slots, and a consumer that holds many items can starve the other
/// streams on the pool. No fairness between them is guaranteed.
///
/// # Memory
///
/// Each slot is `buf_len + 1` bytes rounded up to a multiple of 64, so
/// slots never share a cache line. The slab is `slots * stride + 63`
/// bytes, zeroed once at construction; count all of it as committed memory.
/// One ownership flag per slot adds `slots` bytes.
///
/// Slots are reused **without being re-zeroed**. A slot's bytes beyond the
/// length of the packet it currently holds are whatever an earlier packet
/// left there; [`PacketBuf`] never exposes them, as long as the device's
/// `recv` really writes every byte it reports, as the `recv` contract
/// requires. A device that reports more bytes than it wrote would expose
/// bytes of an earlier packet, possibly one received by another stream on
/// the same pool. That is not undefined behaviour (the slab is always
/// initialised), and the tun-rs backend conforms.
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
pub struct PacketPool {
    inner: PoolRef,
}

impl Clone for PacketPool {
    fn clone(&self) -> Self {
        self.inner.handles.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: self.inner.clone_ref(),
        }
    }
}

impl Drop for PacketPool {
    fn drop(&mut self) {
        // `Release` pairs with `Acquirer::new`'s `Acquire` load. The
        // counted reference in `inner` drops right after this.
        self.inner.handles.fetch_sub(1, Ordering::Release);
    }
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
        // `slots * stride <= u32::MAX` with `stride >= 64`, so every index
        // fits in `u32`. Exact capacity: `into_boxed_slice` does not
        // reallocate, and `FREE != 0` keeps this a plain allocation.
        let mut flags = Vec::with_capacity(slots);
        flags.extend((0..slots).map(|_| AtomicU8::new(FREE)));
        let inner = PoolRef::new(PoolInner {
            slab,
            stride,
            stride_recip: stride_recip(stride),
            buf_len,
            slots,
            flags: flags.into_boxed_slice(),
            last_freed: AtomicU32::new(0),
            refs: AtomicUsize::new(1),
            handles: AtomicUsize::new(1),
            state: Mutex::new(FreeState {
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
    /// pool may change it immediately afterwards. Takes `O(slots)` time: it
    /// counts the free slot flags.
    pub fn available(&self) -> usize {
        self.inner
            .flags
            .iter()
            .filter(|flag| flag.load(Ordering::Acquire) == FREE)
            .count()
    }

    /// Sets `stop` under the pool mutex and wakes every thread parked in
    /// [`Acquirer::acquire_blocking`]. `notify_all` because other workers on
    /// a shared pool may be parked too, and each re-checks its own flag.
    pub(crate) fn stop_worker(&self, stop: &AtomicBool) {
        let st = self.inner.lock();
        stop.store(true, Ordering::Release);
        drop(st);
        self.inner.cv.notify_all();
    }
}

/// Claims `flag` if it is free.
#[inline]
fn claim(flag: &AtomicU8, exclusive: bool) -> bool {
    if exclusive {
        // Unsafe-inventory item (9): only this acquirer ever turns a `FREE`
        // flag into `OWNED`, so the flag cannot change between this load
        // and the store. `Acquire` pairs with the last owner's `Release`
        // store of `FREE`.
        flag.load(Ordering::Acquire) == FREE && {
            flag.store(OWNED, Ordering::Relaxed);
            true
        }
    } else {
        flag.load(Ordering::Relaxed) == FREE
            && flag
                .compare_exchange(FREE, OWNED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
    }
}

/// Claims a free slot of `inner`, leaving `*cursor` on it.
///
/// Tries `*cursor` (the slot this acquirer took last), then the pool's
/// most recently released slot, then every other slot cyclically from
/// `*cursor`. The first two keep a small set of slots in use, and so in
/// cache: a consumer that drops each item at once on the same thread hands
/// the cursor slot straight back; one that drops items on another thread
/// (the bridge) or after holding them returns the hinted slot.
#[inline]
fn scan(inner: &PoolInner, exclusive: bool, cursor: &mut u32) -> Option<u32> {
    if claim(&inner.flags[*cursor as usize], exclusive) {
        return Some(*cursor);
    }
    scan_rest(inner, exclusive, cursor)
}

/// [`scan`] after the cursor slot turned out to be taken.
#[inline(never)]
fn scan_rest(inner: &PoolInner, exclusive: bool, cursor: &mut u32) -> Option<u32> {
    let flags = &inner.flags;
    let slots = flags.len();
    let start = *cursor as usize;
    let hint = inner.last_freed.load(Ordering::Relaxed);
    if hint != *cursor && claim(&flags[hint as usize], exclusive) {
        *cursor = hint;
        return Some(hint);
    }
    let mut idx = start;
    for _ in 1..slots {
        idx += 1;
        if idx == slots {
            idx = 0;
        }
        if claim(&flags[idx], exclusive) {
            // `idx < slots <= u32::MAX`.
            *cursor = idx as u32;
            return Some(idx as u32);
        }
    }
    None
}

/// The only way to claim slots of a pool: one per native stream and one per
/// bridge worker.
///
/// It prepays counted references in batches (one atomic add per
/// [`CREDIT_BATCH`] slots) and keeps a cursor, the last slot it took, where
/// each scan for a free flag starts (see [`scan`] for the order). A
/// consumer that drops each item at once gets the same, cache-hot slot
/// back; one that drops items later or on another thread gets the slot
/// released last.
///
/// An acquirer built from the only [`PacketPool`] handle is *exclusive*:
/// it claims a free slot with a plain store instead of a compare-exchange.
/// That is sound because no second acquirer can ever appear on that pool:
/// handles are made only by cloning a handle, the acquirer holds the only
/// one, and this crate never builds a second acquirer from a handle it
/// clones afterwards (a stream keeps that clone private and only uses it
/// to stop its worker). Otherwise the acquirer is *shared* and claims with
/// a compare-exchange, so any number of them can use the pool at once.
pub(crate) struct Acquirer {
    pool: PacketPool,
    exclusive: bool,
    /// Prepaid counted references not yet given to a `SlotGuard`.
    credit: usize,
    /// Index of the last slot taken; `< slots`.
    cursor: u32,
}

impl Acquirer {
    /// An acquirer for `pool`; exclusive if `pool` is its only handle.
    pub(crate) fn new(pool: PacketPool) -> Self {
        // `Acquire` pairs with `PacketPool::drop`'s `Release`: a handle that
        // was dropped (with any acquirer it belonged to) is really gone.
        let exclusive = pool.inner.handles.load(Ordering::Acquire) == 1;
        #[cfg(test)]
        if let Some(hooks) = pool.hooks() {
            hooks.acquirers.fetch_add(1, Ordering::SeqCst);
            if exclusive {
                hooks.exclusive_acquirers.fetch_add(1, Ordering::SeqCst);
            }
        }
        Self {
            pool,
            exclusive,
            credit: 0,
            cursor: 0,
        }
    }

    /// The pool this acquirer claims from.
    #[inline]
    pub(crate) fn pool(&self) -> &PacketPool {
        &self.pool
    }

    /// Claims a free slot, if there is one: the synchronous fast path.
    #[inline(always)]
    pub(crate) fn try_acquire(&mut self) -> Option<SlotGuard> {
        let idx = self.scan()?;
        Some(self.mint(idx))
    }

    /// Scans at most `slots` flags from the cursor and claims the first
    /// free one.
    #[inline]
    fn scan(&mut self) -> Option<u32> {
        scan(&self.pool.inner, self.exclusive, &mut self.cursor)
    }

    /// Wraps the claimed slot `idx` in a guard paid from the credit.
    #[inline]
    fn mint(&mut self, idx: u32) -> SlotGuard {
        if self.credit == 0 {
            self.refill();
        }
        self.credit -= 1;
        // SAFETY: `credit` was non-zero, and this gives one unit of it up
        // to the new reference.
        let pool = unsafe { self.pool.inner.credit_ref() };
        SlotGuard {
            pool: Some(pool),
            idx,
        }
    }

    #[cold]
    #[inline(never)]
    fn refill(&mut self) {
        self.pool.inner.add_refs(CREDIT_BATCH);
        self.credit = CREDIT_BATCH;
    }

    /// An async acquire of one slot: resolves as soon as a slot is free.
    pub(crate) fn acquire(&mut self) -> Acquire<'_> {
        Acquire {
            acq: self,
            entry: None,
        }
    }

    /// One poll of an async acquire whose waiter entry is `entry`.
    pub(crate) fn poll_acquire(
        &mut self,
        entry: &mut Option<(u64, usize)>,
        cx: &mut Context<'_>,
    ) -> Poll<SlotGuard> {
        if let Some(idx) = self.scan() {
            return Poll::Ready(self.finish_wait(idx, entry));
        }

        // No free slot: clone before locking, register, set `WAITING`, then
        // scan again outside the lock. Either a release's decrement sees
        // `WAITING` and wakes this entry, or its flag store is visible to
        // this scan.
        let waker = cx.waker().clone();
        let inner = &*self.pool.inner;
        let mut st = inner.lock();
        let displaced = register(&mut st, entry, waker);
        inner.arm_waiting();
        drop(st);
        drop(displaced);
        match self.scan() {
            Some(idx) => Poll::Ready(self.finish_wait(idx, entry)),
            None => Poll::Pending,
        }
    }

    /// Takes the waiter entry back, if any, and mints the guard for `idx`.
    fn finish_wait(&mut self, idx: u32, entry: &mut Option<(u64, usize)>) -> SlotGuard {
        if entry.is_some() {
            let own = take_entry(&mut self.pool.inner.lock(), entry);
            drop(own);
        }
        self.mint(idx)
    }

    /// Removes the waiter entry of an abandoned async acquire, if any.
    pub(crate) fn cancel_wait(&self, entry: &mut Option<(u64, usize)>) {
        if entry.is_none() {
            return;
        }
        let waker = take_entry(&mut self.pool.inner.lock(), entry);
        if let Some(waker) = waker {
            drop_guarded(waker);
        }
    }

    /// Blocks the calling thread until a slot is free or `stop` is set.
    ///
    /// `stop` is checked first, and in the slow path under the pool mutex;
    /// [`PacketPool::stop_worker`] sets it under the same mutex before
    /// notifying, so a stop request is never lost.
    pub(crate) fn acquire_blocking(&mut self, stop: &AtomicBool) -> Option<SlotGuard> {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        if let Some(guard) = self.try_acquire() {
            return Some(guard);
        }
        self.acquire_blocking_slow(stop)
    }

    #[cold]
    #[inline(never)]
    fn acquire_blocking_slow(&mut self, stop: &AtomicBool) -> Option<SlotGuard> {
        let inner = &*self.pool.inner;
        let mut st = inner.lock();
        let got = loop {
            if stop.load(Ordering::Acquire) {
                break None;
            }
            // Register, set `WAITING`, then scan again: either a release's
            // decrement sees `WAITING` (and, locking after this thread
            // waits, sees `blocked` and notifies), or its flag store is
            // visible to this scan.
            st.blocked += 1;
            inner.arm_waiting();
            if let Some(idx) = scan(inner, self.exclusive, &mut self.cursor) {
                st.blocked -= 1;
                break Some(idx);
            }
            st = inner.cv.wait(st).unwrap_or_else(PoisonError::into_inner);
            st.blocked -= 1;
        };
        // Pass a notify on if this thread may have consumed one that another
        // parked thread needs.
        let pass_on = st.blocked > 0 && inner.any_free();
        drop(st);
        if pass_on {
            inner.notify_one();
        }
        got.map(|idx| self.mint(idx))
    }

    #[cfg(test)]
    pub(crate) fn is_exclusive(&self) -> bool {
        self.exclusive
    }
}

impl Drop for Acquirer {
    fn drop(&mut self) {
        if self.credit > 0 {
            // Never the last reference: this acquirer's own handle drops
            // after this.
            self.pool
                .inner
                .refs
                .fetch_sub(self.credit, Ordering::Release);
        }
    }
}

/// Future returned by [`Acquirer::acquire`]. Holds at most one waiter
/// entry, which its drop removes.
pub(crate) struct Acquire<'a> {
    acq: &'a mut Acquirer,
    entry: Option<(u64, usize)>,
}

impl Future for Acquire<'_> {
    type Output = SlotGuard;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SlotGuard> {
        let this = self.get_mut();
        this.acq.poll_acquire(&mut this.entry, cx)
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        self.acq.cancel_wait(&mut self.entry);
    }
}

/// Exclusive ownership of one slot between acquire and hand-off. Releases
/// the slot on drop unless converted into a [`PacketBuf`].
pub(crate) struct SlotGuard {
    /// `Some` until [`SlotGuard::into_buf`] moves it into the view.
    pool: Option<PoolRef>,
    idx: u32,
}

impl SlotGuard {
    #[inline]
    fn pool(&self) -> &PoolInner {
        self.pool
            .as_deref()
            .expect("a live SlotGuard always owns its pool")
    }

    /// The whole slot: exactly `buf_len` bytes, for `recv` to write into.
    #[inline]
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        let pool = self.pool();
        let ptr = pool.view_ptr(self.idx as usize * pool.stride);
        let len = pool.buf_len;
        // SAFETY: `idx < slots` (it was claimed from its flag), so
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
    #[inline]
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
    #[inline]
    fn drop(&mut self) {
        // A guard turned into a `PacketBuf` is empty, so this is only a
        // check on the receive path; releasing an unused slot is out of line.
        if let Some(pool) = self.pool.take() {
            release_unused(pool, self.idx);
        }
    }
}

/// Returns a slot that never became a `PacketBuf` (a receive error or an
/// overlong length).
#[cold]
#[inline(never)]
fn release_unused(pool: PoolRef, idx: u32) {
    // Flag store first, then the decrement as `pool` drops.
    pool.release(idx);
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
    /// One counted reference to the pool state.
    pool: PoolRef,
    off: u32,
    len: u32,
}

impl PacketBuf {
    /// Number of bytes in the view.
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the view is empty.
    #[inline]
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

    #[inline]
    fn slot(&self) -> u32 {
        // `off < slots * stride <= u32::MAX`; equals `off / stride`.
        div_stride(self.off, self.pool.stride_recip)
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

    #[inline]
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
    #[inline]
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
    #[inline]
    fn drop(&mut self) {
        // Flag store first; the counted reference in `pool` drops right
        // after this, and wakes waiters if it sees `WAITING`.
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

/// A one-shot test hook.
#[cfg(test)]
pub(crate) type Hook = Mutex<Option<Box<dyn FnOnce() + Send>>>;

/// Per-pool test hooks.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    /// Number of `notify_one` calls made by release and by
    /// `acquire_blocking`'s pass-on.
    pub(crate) notify_count: AtomicUsize,
    /// Number of releases that took the slow path (drained live waiters).
    pub(crate) drain_count: AtomicUsize,
    /// Runs once, in the slow path of release, after the drained wakers
    /// were woken and before the pool mutex is locked again.
    pub(crate) release_between_unlock_and_relock: Hook,
    /// Runs once in the bridge worker's exit, right after its channel
    /// sender is dropped and before the final wake.
    pub(crate) after_tx_drop: Hook,
    /// Runs once in the bridge worker's exit, right after the final wake
    /// and before its `Arc<BridgeWaker>` is released.
    pub(crate) after_worker_wake: Hook,
    /// Runs once in the bridge worker, right after it sent a terminal error
    /// and before it leaves its loop.
    pub(crate) after_terminal_send: Hook,
    /// Set when `PoolInner` (and so the slab) is dropped.
    pub(crate) inner_dropped: AtomicBool,
    /// Number of times `PoolInner` was dropped (must never exceed 1).
    pub(crate) inner_drops: AtomicUsize,
    /// Number of `Acquirer`s built on the pool.
    pub(crate) acquirers: AtomicUsize,
    /// How many of them were exclusive.
    pub(crate) exclusive_acquirers: AtomicUsize,
}

/// Takes and runs a one-shot hook, if one is set.
#[cfg(test)]
pub(crate) fn run_hook(hook: &Hook) {
    let hook = hook.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
impl TestHooks {
    /// Installs `hook` in `slot`.
    pub(crate) fn set(slot: &Hook, hook: impl FnOnce() + Send + 'static) {
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }
}

#[cfg(test)]
impl PacketPool {
    pub(crate) fn with_hooks(slots: usize, buf_len: usize, hooks: Arc<TestHooks>) -> Result<Self> {
        let (stride, _total, layout) = layout_for(slots, buf_len).ok_or(Error::InvalidState)?;
        Ok(Self::build(slots, buf_len, stride, layout, Some(hooks)))
    }

    /// The pool's hooks, if it was built with [`Self::with_hooks`].
    pub(crate) fn hooks(&self) -> Option<&TestHooks> {
        self.inner.hooks.as_deref()
    }

    fn stride(&self) -> usize {
        self.inner.stride
    }

    fn base_addr(&self) -> usize {
        self.inner.slab.base.as_ptr().addr()
    }

    fn is_free(&self, idx: usize) -> bool {
        self.inner.flags[idx].load(Ordering::SeqCst) == FREE
    }

    /// The reference count, without the `WAITING` bit.
    fn ref_count(&self) -> usize {
        self.inner.refs.load(Ordering::SeqCst) & COUNT
    }

    fn handles(&self) -> usize {
        self.inner.handles.load(Ordering::SeqCst)
    }

    fn waiting_bit(&self) -> bool {
        self.inner.refs.load(Ordering::SeqCst) & WAITING != 0
    }

    fn set_waiting_bit(&self) {
        self.inner.arm_waiting();
    }

    /// Test shim: blocks for a slot through a fresh, shared acquirer.
    pub(crate) fn acquire_blocking(&self, stop: &AtomicBool) -> Option<SlotGuard> {
        Acquirer::new(self.clone()).acquire_blocking(stop)
    }

    /// Test shim: an async acquire through a fresh, shared acquirer that it
    /// owns, so it can be stored with a `'static` lifetime.
    fn acquire(&self) -> OwnedAcquire {
        OwnedAcquire {
            acq: Acquirer::new(self.clone()),
            entry: None,
        }
    }

    fn waiters_len(&self) -> usize {
        self.inner.lock().waiters.len()
    }

    fn live(&self) -> usize {
        self.inner.lock().live
    }

    pub(crate) fn blocked(&self) -> u32 {
        self.inner.lock().blocked
    }

    fn draining(&self) -> bool {
        self.inner.lock().draining
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
        let pool = self.clone();
        let result = catch_unwind(AssertUnwindSafe(move || {
            let _guard = pool.inner.state.lock();
            panic!("poisoning the pool mutex on purpose");
        }));
        assert!(result.is_err());
    }
}

/// An async acquire that owns its [`Acquirer`] (test only).
#[cfg(test)]
pub(crate) struct OwnedAcquire {
    acq: Acquirer,
    entry: Option<(u64, usize)>,
}

#[cfg(test)]
impl Future for OwnedAcquire {
    type Output = SlotGuard;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SlotGuard> {
        let this = self.get_mut();
        this.acq.poll_acquire(&mut this.entry, cx)
    }
}

#[cfg(test)]
impl Drop for OwnedAcquire {
    fn drop(&mut self) {
        self.acq.cancel_wait(&mut self.entry);
    }
}

/// A test waker whose `clone` panics while `panic_on_clone` is set. A
/// `Wake` impl's clone is an `Arc` clone and cannot panic, so this needs a
/// hand-written vtable; it lives here with the crate's other `unsafe` code.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct CloneSwitch {
    pub(crate) panic_on_clone: AtomicBool,
}

#[cfg(test)]
impl CloneSwitch {
    pub(crate) fn waker(self: &Arc<Self>) -> Waker {
        let data = Arc::into_raw(Arc::clone(self)).cast::<()>();
        // SAFETY: `data` owns one strong count of an `Arc<CloneSwitch>`, and
        // every vtable function below treats it exactly that way.
        unsafe { Waker::from_raw(std::task::RawWaker::new(data, &CLONE_SWITCH_VTABLE)) }
    }
}

#[cfg(test)]
static CLONE_SWITCH_VTABLE: std::task::RawWakerVTable = std::task::RawWakerVTable::new(
    clone_switch_clone,
    clone_switch_wake,
    clone_switch_wake_by_ref,
    clone_switch_drop,
);

#[cfg(test)]
unsafe fn clone_switch_clone(data: *const ()) -> std::task::RawWaker {
    // SAFETY: `data` came from `Arc::into_raw` and the waker being cloned
    // still owns its strong count, so the `CloneSwitch` is alive.
    let switch = unsafe { &*data.cast::<CloneSwitch>() };
    if switch.panic_on_clone.load(Ordering::SeqCst) {
        panic!("test waker panics on clone");
    }
    // SAFETY: as above; the new count is owned by the returned waker.
    unsafe { Arc::increment_strong_count(data.cast::<CloneSwitch>()) };
    std::task::RawWaker::new(data, &CLONE_SWITCH_VTABLE)
}

#[cfg(test)]
unsafe fn clone_switch_wake(data: *const ()) {
    // SAFETY: waking by value consumes the waker's own strong count.
    unsafe { clone_switch_drop(data) }
}

#[cfg(test)]
unsafe fn clone_switch_wake_by_ref(_data: *const ()) {}

#[cfg(test)]
unsafe fn clone_switch_drop(data: *const ()) {
    // SAFETY: `data` came from `Arc::into_raw` and this waker owns one
    // strong count, released exactly once here.
    drop(unsafe { Arc::from_raw(data.cast::<CloneSwitch>()) });
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
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

    fn poll(fut: &mut (impl Future<Output = SlotGuard> + Unpin), waker: &Waker) -> Poll<SlotGuard> {
        Pin::new(fut).poll(&mut Context::from_waker(waker))
    }

    /// Records wakes and unparks the thread that waits for one.
    struct Unpark {
        woken: AtomicBool,
        thread: thread::Thread,
    }

    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.woken.store(true, Ordering::SeqCst);
            self.thread.unpark();
        }
    }

    /// Polls `fut` to completion on this thread, parking between polls.
    /// Panics if a pending poll is not followed by a wake within
    /// [`DEADLINE`]: that is a lost wake-up.
    fn block_on_bounded(fut: &mut (impl Future<Output = SlotGuard> + Unpin)) -> SlotGuard {
        let unpark = Arc::new(Unpark {
            woken: AtomicBool::new(false),
            thread: thread::current(),
        });
        let waker = Waker::from(Arc::clone(&unpark));
        loop {
            unpark.woken.store(false, Ordering::SeqCst);
            if let Poll::Ready(guard) = poll(fut, &waker) {
                return guard;
            }
            let start = Instant::now();
            while !unpark.woken.load(Ordering::SeqCst) {
                let left = DEADLINE
                    .checked_sub(start.elapsed())
                    .expect("lost wake-up: a pending acquire was never woken");
                thread::park_timeout(left);
            }
        }
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

    /// `div_stride` equals `off / stride` around every slot boundary and at
    /// the extremes of the `u32` range, for small, typical, odd and maximal
    /// strides.
    #[test]
    fn div_stride_is_exact() {
        let max = u32::MAX as usize;
        let strides = [
            64,
            128,
            1536,
            9024,
            65_600,
            1 << 20,
            (max / 3) & !63,
            max & !63,
            max,
        ];
        for stride in strides {
            let recip = stride_recip(stride);
            let check = |off: u32| {
                assert_eq!(
                    div_stride(off, recip) as usize,
                    off as usize / stride,
                    "{off} / {stride}"
                );
            };
            let mut probes = vec![0, 1, u32::MAX - 1, u32::MAX];
            let boundaries = if cfg!(miri) { 4 } else { 1024 };
            for k in 1..=boundaries.min(max / stride) {
                let base = (k * stride) as u32;
                probes.extend([base - 1, base, base.saturating_add(1)]);
            }
            let top = (max / stride * stride) as u32;
            probes.extend([top - 1, top]);
            probes.into_iter().for_each(check);
        }
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
            assert!(pool.is_free(1) && !pool.is_free(0));
            drop(first);
            assert!(pool.is_free(0));
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

    /// The release fast path never locks, so each release here has a waiter
    /// parked first: registration and the release's slow path both go
    /// through the poisoned mutex.
    #[test]
    fn pool_tolerates_a_poisoned_mutex() {
        let pool = PacketPool::new(1, 16).unwrap();
        let buf = take_buf(&pool, 4);
        pool.poison();
        assert!(pool.is_poisoned());
        assert_eq!(pool.available(), 0);
        let (count, waker) = Counter::waker();
        let mut fut = pool.acquire();
        assert!(poll(&mut fut, &waker).is_pending());
        drop(buf);
        assert_eq!(count.count(), 1, "the slow path woke the waiter");
        assert!(poll(&mut fut, &waker).is_ready());
        assert_eq!(pool.available(), 1);

        // A release while unwinding does not panic again (which would abort).
        let buf = take_buf(&pool, 4);
        let (count, waker) = Counter::waker();
        let mut fut = pool.acquire();
        assert!(poll(&mut fut, &waker).is_pending());
        let result = catch_unwind(AssertUnwindSafe(move || {
            let _buf = buf;
            panic!("unwinding with a PacketBuf alive");
        }));
        assert!(result.is_err());
        assert_eq!(count.count(), 1);
        assert_eq!(pool.available(), 1);
        drop(fut);
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
        // `release` asserts in test builds that a slot is never released
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

        type Stash = Arc<Mutex<Vec<(OwnedAcquire, SlotGuard)>>>;
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
        // A long-lived acquirer: dropping one would drop a handle, whose
        // decrement would see the stale `WAITING` bit and compact early.
        let mut acq = Acquirer::new(pool.clone());
        let mut fut = acq.acquire();
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

    /// A panic payload whose own `Drop` panics.
    struct PanickyPayload;

    impl Drop for PanickyPayload {
        fn drop(&mut self) {
            panic!("test panic payload panics on drop");
        }
    }

    /// Panics with a [`PanickyPayload`] when woken.
    struct WakePanicsWithPanickyPayload;

    impl Wake for WakePanicsWithPanickyPayload {
        fn wake(self: Arc<Self>) {
            std::panic::panic_any(PanickyPayload);
        }
    }

    /// A caught wake panic whose payload panics again on drop escapes the
    /// release (a documented residual), but the unwind guard in the drain
    /// clears `draining`, so later waiters are still woken.
    #[test]
    fn pool_escaping_payload_panic_does_not_leave_the_pool_draining() {
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(1, 16, Arc::clone(&hooks)).unwrap();
        let held = take_buf(&pool, 1);
        let mut bad = pool.acquire();
        let bad_waker = Waker::from(Arc::new(WakePanicsWithPanickyPayload));
        assert!(poll(&mut bad, &bad_waker).is_pending());
        drop(bad_waker);

        let escaped = catch_unwind(AssertUnwindSafe(move || drop(held)));
        assert!(escaped.is_err(), "the payload's own panic escapes");
        assert!(!pool.draining(), "the unwind guard cleared `draining`");
        assert_eq!(pool.available(), 1);
        drop(bad);

        let held = take_buf(&pool, 1);
        let (count, waker) = Counter::waker();
        let mut fut = pool.acquire();
        assert!(poll(&mut fut, &waker).is_pending());
        drop(held);
        assert_eq!(count.count(), 1, "a later waiter is woken");
        assert_eq!(hooks.drain_count.load(Ordering::SeqCst), 2);
        assert!(poll(&mut fut, &waker).is_ready());
        assert_eq!(pool.available(), 1);
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
                // One shared acquirer per thread: the main thread keeps a
                // handle, so none is exclusive.
                let mut acq = Acquirer::new(pool.clone());
                assert!(!acq.is_exclusive());
                thread::spawn(move || {
                    let tag = t as u8 + 1;
                    for round in 0..rounds {
                        let mut guard = if (t + round) % 2 == 0 {
                            acq.acquire_blocking(&never()).expect("never stopped")
                        } else {
                            futures::executor::block_on(acq.acquire())
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
        assert_eq!(pool.ref_count(), 1, "every credit and view was returned");
    }

    // (a) The exclusive mode is decided once, from the handle count.

    #[test]
    fn acquirer_is_exclusive_only_with_the_sole_handle() {
        let pool = PacketPool::new(2, 16).unwrap();
        assert!(!Acquirer::new(pool.clone()).is_exclusive());
        let clone = pool.clone();
        let shared = Acquirer::new(pool);
        assert!(!shared.is_exclusive());
        drop(clone);
        assert!(!shared.is_exclusive(), "decided once, at construction");

        let pool = shared.pool().clone();
        drop(shared);
        assert_eq!(pool.handles(), 1);
        let exclusive = Acquirer::new(pool);
        assert!(exclusive.is_exclusive());
    }

    // (b) Scan order: the cursor slot, then the slot released last, then
    // forward from the cursor with wrap-around.

    #[test]
    fn acquirer_reuses_the_slot_dropped_last() {
        for shared in [false, true] {
            let pool = PacketPool::new(4, 16).unwrap();
            let keep = shared.then(|| pool.clone());
            let mut acq = Acquirer::new(pool);
            assert_eq!(acq.is_exclusive(), !shared);
            for _ in 0..3 {
                let a = acq.try_acquire().unwrap();
                assert_eq!(a.idx, 0, "the cursor slot comes back");
                drop(a);
            }
            let held = acq.try_acquire().unwrap();
            let next = acq.try_acquire().unwrap();
            assert_eq!((held.idx, next.idx), (0, 1), "held slot is skipped");
            drop(held);
            let mut taken: Vec<_> = (0..3).map(|_| acq.try_acquire().unwrap()).collect();
            let indices: Vec<_> = taken.iter().map(|guard| guard.idx).collect();
            assert_eq!(indices, [0, 2, 3], "the slot released last comes first");
            assert!(acq.try_acquire().is_none(), "every slot is taken");

            // Cursor 3 is held, so the hint (1) wins; then cursor 1 is held,
            // the hint is the cursor, and the scan wraps from 1 to 0.
            drop(taken.remove(0));
            drop(next);
            let hinted = acq.try_acquire().unwrap();
            let wrapped = acq.try_acquire().unwrap();
            assert_eq!((hinted.idx, wrapped.idx), (1, 0), "the scan wraps around");
            assert!(acq.try_acquire().is_none(), "every slot is taken");
            drop(keep);
        }
    }

    // (c) Reference-count accounting and the cross-thread final free.

    #[test]
    fn pool_reference_count_balances_and_frees_once() {
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(4, 16, Arc::clone(&hooks)).unwrap();
        let mut acq = Acquirer::new(pool.clone());
        let mut bufs: Vec<_> = (0..3)
            .map(|_| acq.try_acquire().unwrap().into_buf(1).unwrap())
            .collect();
        assert_eq!(pool.handles(), 2);
        assert_eq!(pool.ref_count(), 2 + 3 + (CREDIT_BATCH - 3));
        drop(acq);
        assert_eq!(pool.handles(), 1);
        assert_eq!(pool.ref_count(), 1 + 3, "credit returned: handles + views");

        drop(pool);
        let last = bufs.pop().unwrap();
        drop(bufs);
        assert!(!hooks.inner_dropped.load(Ordering::SeqCst));
        thread::spawn(move || drop(last)).join().unwrap();
        assert_eq!(hooks.inner_drops.load(Ordering::SeqCst), 1);
    }

    // (d) The WAITING handshake loses no wake-up, in both modes.

    fn waiting_handshake_stress(slots: usize, shared: bool) {
        let iterations = if cfg!(miri) { 50 } else { 1_000 };
        let pool = PacketPool::new(slots, 16).unwrap();
        let keep = shared.then(|| pool.clone());
        let mut acq = Acquirer::new(pool);
        assert_eq!(acq.is_exclusive(), !shared);
        let (tx, rx) = mpsc::channel::<SlotGuard>();
        let releaser = thread::spawn(move || {
            for (i, guard) in rx.into_iter().enumerate() {
                if i % 2 == 0 {
                    thread::yield_now();
                }
                drop(guard);
            }
        });
        for _ in 0..iterations {
            let mut fut = acq.acquire();
            let guard = block_on_bounded(&mut fut);
            drop(fut);
            tx.send(guard).unwrap();
        }
        drop(tx);
        releaser.join().unwrap();
        let pool = acq.pool().clone();
        drop(acq);
        assert_eq!(pool.available(), slots);
        assert_eq!(pool.ref_count(), 1 + usize::from(shared));
        drop(keep);
    }

    #[test]
    fn pool_waiting_handshake_exclusive_one_slot() {
        waiting_handshake_stress(1, false);
    }

    #[test]
    fn pool_waiting_handshake_exclusive_two_slots() {
        waiting_handshake_stress(2, false);
    }

    #[test]
    fn pool_waiting_handshake_shared_one_slot() {
        waiting_handshake_stress(1, true);
    }

    #[test]
    fn pool_waiting_handshake_shared_two_slots() {
        waiting_handshake_stress(2, true);
    }

    // (e) A stale WAITING bit on the last reference frees exactly once.

    #[test]
    fn pool_stale_waiting_bit_on_the_last_reference_frees_once() {
        // The last reference is a slot guard; the bit is left by a waiter
        // that gave up.
        let hooks = Arc::new(TestHooks::default());
        let mut acq = Acquirer::new(PacketPool::with_hooks(1, 16, Arc::clone(&hooks)).unwrap());
        let guard = acq.try_acquire().unwrap();
        let mut fut = acq.acquire();
        assert!(poll(&mut fut, Waker::noop()).is_pending());
        drop(fut);
        assert!(acq.pool().waiting_bit());
        drop(acq);
        assert!(!hooks.inner_dropped.load(Ordering::SeqCst));
        drop(guard);
        assert_eq!(hooks.inner_drops.load(Ordering::SeqCst), 1);

        // The last reference is a handle.
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(1, 16, Arc::clone(&hooks)).unwrap();
        pool.set_waiting_bit();
        drop(pool);
        assert_eq!(hooks.inner_drops.load(Ordering::SeqCst), 1);
    }

    // (f) Parked workers are notified on every release while one waits.

    #[test]
    fn pool_notifies_every_release_while_workers_are_parked() {
        let hooks = Arc::new(TestHooks::default());
        let pool = PacketPool::with_hooks(2, 16, Arc::clone(&hooks)).unwrap();
        let first = take_buf(&pool, 1);
        let second = take_buf(&pool, 1);
        // Each worker returns its acquirer too: dropping it in the thread
        // would drop a handle, whose decrement may see the re-armed
        // `WAITING` bit and add one (harmless) spurious notify.
        let spawn = || {
            let mut acq = Acquirer::new(pool.clone());
            thread::spawn(move || {
                let guard = acq.acquire_blocking(&never());
                (acq, guard)
            })
        };
        let (a, b) = (spawn(), spawn());
        wait_until("both workers to park", || pool.blocked() == 2);

        drop(first);
        wait_until("one worker to get a slot", || {
            a.is_finished() || b.is_finished()
        });
        drop(second);
        let workers = [a.join().unwrap(), b.join().unwrap()];
        assert!(workers.iter().all(|(_, guard)| guard.is_some()));
        assert_eq!(hooks.notify_count.load(Ordering::SeqCst), 2);
        drop(workers);
        assert_eq!(hooks.notify_count.load(Ordering::SeqCst), 2);
        assert_eq!(pool.available(), 2);
    }

    // (g) The test-build double-release detector.

    #[test]
    #[should_panic(expected = "released twice")]
    fn pool_double_release_is_detected() {
        let pool = PacketPool::new(1, 16).unwrap();
        // Slot 0 is free, so this guard's release is a second release.
        let guard = SlotGuard {
            pool: Some(pool.inner.clone_ref()),
            idx: 0,
        };
        drop(guard);
    }
}
