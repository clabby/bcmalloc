//! How a thread finds its heap.
//!
//! The allocation fast path wants the calling thread's heap in a couple of instructions, and a
//! thread's heap has to go back to the pool when the thread exits. Both come from one POSIX
//! thread-specific key per allocator. The key's destructor releases the heap at thread exit, and a
//! cheaper copy of the same pointer serves the lookups.
//!
//! ```text
//!    fast path                          thread exit
//!    +-----------------------+          +--------------------------------------+
//!    | read the thread slot  |          | pthread runs the key destructor      |
//!    | null?  go to the slow |          |   release the heap's claim           |
//!    | path and bind a heap  |          |   remember that this thread is gone  |
//!    +-----------------------+          +--------------------------------------+
//! ```
//!
//! # The two lookups
//!
//! On Apple silicon the slot is read straight from the thread's key table, which the CPU register
//! for thread-specific data points at. That is the same load that the system's own
//! getspecific routine does, without the call. Creating a key checks that the layout is what this
//! code expects by storing a value through the public interface and reading it back both ways, and
//! it refuses the key if they disagree. Everywhere else, and under Miri, the pointer sits in an
//! ordinary thread-local that is initialised at compile time. A plain thread-local without a
//! destructor needs no lazy set-up, so it costs a few instructions on Linux.
//!
//! # Threads that are going away
//!
//! Once a thread's destructor has run, other destructors on that thread may still allocate or
//! free. The slot is empty by then, and binding a new heap would hand a claim to a thread that will
//! never release it. [`exiting`] tells the slow path to use a temporary heap instead.

use core::{cell::Cell, ffi::c_void, ptr};

use crate::heap::HeapData;

/// A POSIX thread-specific key.
pub(crate) type Key = usize;

/// A key value that no key has, standing for an allocator that has not created its key yet.
pub(crate) const NO_KEY: Key = usize::MAX;

thread_local! {
    /// Set once this thread's heap has been released at exit.
    static EXITED: Cell<bool> = const { Cell::new(false) };
    /// Set while this thread is binding a heap. When the allocator replaces malloc, creating a key
    /// or storing a value may itself allocate, and that allocation must not try to bind again.
    static BINDING: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is in the middle of binding a heap.
pub(crate) fn binding() -> bool {
    BINDING.with(Cell::get)
}

/// Marks this thread as binding a heap, or as done.
pub(crate) fn set_binding(on: bool) {
    BINDING.with(|binding| binding.set(on));
}

/// Whether this thread has already given its heap back, so that it must not bind a new one.
pub(crate) fn exiting() -> bool {
    EXITED.with(Cell::get)
}

/// Creates a key whose destructor releases the heap stored under it, or returns None if the
/// system has no key to give or the slot layout is not what the fast lookup assumes.
pub(crate) fn create_key() -> Option<Key> {
    let mut key = 0;
    // SAFETY: the destructor has the signature that pthread expects.
    if unsafe { libc::pthread_key_create(&raw mut key, Some(release_heap)) } != 0 {
        return None;
    }
    let key = key as Key;
    if !slot::probe(key) {
        // SAFETY: the key was created above and nothing has used it.
        unsafe { libc::pthread_key_delete(key as _) };
        return None;
    }
    Some(key)
}

/// Deletes a key that no thread has bound a heap under.
pub(crate) fn delete_key(key: Key) {
    // SAFETY: the caller never published the key, so no thread holds a value under it.
    unsafe { libc::pthread_key_delete(key as _) };
}

/// The heap that this thread bound under the key, or null.
#[inline(always)]
pub(crate) fn get(key: Key) -> *const HeapData {
    slot::get(key)
}

/// As [`get`], but where the fast lookup cannot tell, it asks pthread. The fast paths call [`get`]
/// and come here only when that returns null, so they make no call of their own.
pub(crate) fn lookup(key: Key) -> *const HeapData {
    slot::lookup(key)
}

/// Binds the heap as this thread's under the key, so that the key destructor releases it when the
/// thread exits. Returns false, and binds nothing, if the system refuses the value.
pub(crate) fn bind(key: Key, data: &'static HeapData) -> bool {
    // SAFETY: the key is valid, and the value is a heap that stays alive for the destructor.
    if unsafe { libc::pthread_setspecific(key as _, ptr::from_ref(data).cast()) } != 0 {
        return false;
    }
    slot::set(key, data);
    true
}

/// The key destructor. pthread has already cleared the thread's slot when it calls this.
unsafe extern "C" fn release_heap(value: *mut c_void) {
    if value.is_null() {
        return;
    }
    // SAFETY: only bind stores values under the key, and they are live heap slots.
    unsafe { (*value.cast::<HeapData>()).give_up() };
    slot::clear();
    EXITED.with(|exited| exited.set(true));
}

#[cfg(all(
    target_vendor = "apple",
    target_arch = "aarch64",
    not(miri),
    not(bcmalloc_portable_tls)
))]
mod slot {
    use core::{arch::asm, ptr};

    use super::Key;
    use crate::heap::HeapData;

    /// The table of this thread's key slots.
    #[inline(always)]
    fn table() -> *const *const HeapData {
        let register: usize;
        // SAFETY: reading the thread-specific data register has no effects.
        unsafe {
            asm!(
                "mrs {}, tpidrro_el0",
                out(reg) register,
                options(nomem, nostack, pure, preserves_flags)
            );
        }
        // The low bits of the register carry other information.
        ptr::with_exposed_provenance(register & !7)
    }

    #[inline(always)]
    pub(super) fn get(key: Key) -> *const HeapData {
        if key == super::NO_KEY {
            return ptr::null();
        }
        // SAFETY: the key was created by create_key and was checked by probe, so its slot is in
        // the table.
        unsafe { *table().add(key) }
    }

    pub(super) fn lookup(key: Key) -> *const HeapData {
        get(key)
    }

    pub(super) const fn set(_key: Key, _data: &HeapData) {}

    pub(super) const fn clear() {}

    /// Stores a marker through pthread, reads it through the table and clears it again.
    pub(super) fn probe(key: Key) -> bool {
        static MARKER: u8 = 0;
        let marker: *const u8 = &raw const MARKER;
        // SAFETY: the key is fresh, so nothing else uses its slot on this thread.
        unsafe { libc::pthread_setspecific(key as _, marker.cast()) };
        let found = get(key).cast::<u8>() == marker;
        // SAFETY: as above. Clearing the slot keeps the destructor from running on the marker.
        unsafe { libc::pthread_setspecific(key as _, ptr::null()) };
        found
    }
}

#[cfg(not(all(
    target_vendor = "apple",
    target_arch = "aarch64",
    not(miri),
    not(bcmalloc_portable_tls)
)))]
mod slot {
    use core::{cell::Cell, ptr};

    use super::Key;
    use crate::heap::HeapData;

    /// The key of an empty slot, which no allocator has. Pthread keys are small, and an allocator
    /// without one has [`NO_KEY`](super::NO_KEY).
    const EMPTY: Key = super::NO_KEY - 1;

    thread_local! {
        /// The key and heap this thread used last. A thread that serves several allocators keeps
        /// only the latest here, and pthread remembers the others. A slot whose key matches always
        /// holds a heap.
        static CURRENT: Cell<(Key, *const HeapData)> = const { Cell::new((EMPTY, ptr::null())) };
    }

    #[inline(always)]
    pub(super) fn get(key: Key) -> *const HeapData {
        let (bound, data) = CURRENT.with(Cell::get);
        if bound == key {
            // SAFETY: only `set` stores a key that an allocator has, and it stores a heap with it.
            // Knowing that lets the caller's null check fold into the key check.
            unsafe { core::hint::assert_unchecked(!data.is_null()) };
            data
        } else {
            ptr::null()
        }
    }

    /// Looks the heap up through pthread and makes it the latest.
    pub(super) fn lookup(key: Key) -> *const HeapData {
        if key == super::NO_KEY {
            return ptr::null();
        }
        // SAFETY: the key was created by create_key.
        let data = unsafe { libc::pthread_getspecific(key as _) }.cast::<HeapData>().cast_const();
        if !data.is_null() {
            CURRENT.with(|current| current.set((key, data)));
        }
        data
    }

    pub(super) fn set(key: Key, data: &HeapData) {
        CURRENT.with(|current| current.set((key, ptr::from_ref(data))));
    }

    pub(super) fn clear() {
        CURRENT.with(|current| current.set((EMPTY, ptr::null())));
    }

    pub(super) const fn probe(_key: Key) -> bool {
        true
    }
}

#[cfg(all(test, not(loom)))]
mod test {
    use std::thread;

    use super::*;
    use crate::testing::TestRegion;

    #[test]
    fn a_thread_finds_the_heap_it_bound_and_others_do_not() {
        let test = TestRegion::new(8, 2);
        let region = test.region();
        let key = create_key().expect("a key");
        assert!(get(key).is_null());

        let data = region.heap().expect("a free heap").leak();
        // SAFETY: the test keeps the region alive for as long as the heap is bound.
        assert!(bind(key, unsafe { &*ptr::from_ref(data) }));
        assert_eq!(get(key), ptr::from_ref(data));
        thread::scope(|scope| {
            scope.spawn(|| assert!(get(key).is_null()));
        });

        // Unbind without running the destructor on this long-lived thread.
        // SAFETY: the key is valid.
        unsafe { libc::pthread_setspecific(key as _, ptr::null()) };
        slot::clear();
        data.release_claim();
        delete_key(key);
    }

    #[test]
    fn exiting_threads_release_their_heap_and_know_they_are_gone() {
        let test = TestRegion::new(8, 2);
        let region = test.region();
        let key = create_key().expect("a key");
        let data = region.heap().expect("a free heap").leak();
        assert!(data.is_claimed());
        struct Handoff(*const HeapData);
        // SAFETY: the heap slot outlives the thread, and only the heap's claim is exclusive.
        unsafe impl Send for Handoff {}
        let handoff = Handoff(ptr::from_ref(data));
        // Joining the handle waits for the operating system thread, and so for its destructors.
        // A scoped thread counts as finished as soon as its closure returns.
        thread::spawn(move || {
            let handoff = handoff;
            assert!(!exiting());
            // SAFETY: the test keeps the region alive until the thread has been joined.
            assert!(bind(key, unsafe { &*handoff.0 }));
            assert_eq!(get(key), handoff.0);
        })
        .join()
        .unwrap();
        assert!(!data.is_claimed(), "the destructor must release the claim");
        assert!(!exiting(), "only the exited thread is marked");
        delete_key(key);
    }
}
