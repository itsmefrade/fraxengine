//! The threading primitives the decoder uses (locks, condition variables,
//! channels, scoped and long-lived threads).
//!
//! Native builds use `std` unchanged. With the `wasm-threads` feature on
//! wasm32 the module is built for shared memory (`+atomics`, imported shared
//! `WebAssembly.Memory`) on a stable toolchain whose prebuilt std has no
//! thread support: its `Mutex`, `Condvar` and `OnceLock` are single-thread
//! stubs and `std::thread::spawn` fails. So the primitives are implemented
//! here on atomics plus two host imports (`Atomics.wait` and
//! `Atomics.notify`, wasm's futex), and "threads" are host workers started
//! in advance, each running [`thread_main`] on its own instance of the module
//! (same memory, own stack). Scoped tasks run on an idle worker or, when none
//! is idle, inline on the spawning thread, so the worker count never affects
//! the result or can deadlock a decode; only the wavefront rows, which wait
//! on each other, claim their workers up front (`claim_helpers`).

#[cfg(not(all(target_arch = "wasm32", feature = "wasm-threads")))]
mod imp {
    pub(crate) use std::sync::{Condvar, Mutex, OnceLock, mpsc};
    pub(crate) use std::thread::{Scope, ScopedJoinHandle, scope};

    /// A thread reserved for one scoped task. Native threads are started on
    /// demand, so there is nothing to reserve.
    pub(crate) struct Helper;

    /// Reserve threads for `n` scoped tasks that wait on each other.
    pub(crate) fn claim_helpers(n: usize) -> Vec<Helper> {
        (0..n).map(|_| Helper).collect()
    }

    /// Spawn a scoped task on a reserved thread (`None`: any thread).
    pub(crate) fn spawn_on<'scope, 'env, T: Send + 'scope>(
        s: &'scope Scope<'scope, 'env>,
        _helper: Option<Helper>,
        f: impl FnOnce() -> T + Send + 'scope,
    ) -> ScopedJoinHandle<'scope, T> {
        s.spawn(f)
    }

    /// A long-lived thread (frame threading's picture workers).
    pub(crate) fn spawn_thread(
        name: &str,
        f: impl FnOnce() + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name(name.to_string())
            .spawn(f)
            .expect("spawn a worker thread")
    }
}

#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
mod imp {
    use std::cell::UnsafeCell;
    use std::convert::Infallible;
    use std::marker::PhantomData;
    use std::mem::MaybeUninit;
    use std::ops::{Deref, DerefMut};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        /// `Atomics.wait` on the 32-bit word at `addr` while it equals `expected`.
        fn aether_futex_wait(addr: *const AtomicU32, expected: u32);
        /// `Atomics.notify` of up to `count` waiters on `addr`.
        fn aether_futex_wake(addr: *const AtomicU32, count: u32);
    }

    fn futex_wait(a: &AtomicU32, expected: u32) {
        unsafe { aether_futex_wait(a, expected) }
    }

    fn futex_wake(a: &AtomicU32, count: u32) {
        unsafe { aether_futex_wake(a, count) }
    }

    // ---- Mutex ----------------------------------------------------------

    /// 0 unlocked, 1 locked, 2 locked with waiters (the futex mutex std uses
    /// on wasm with atomics).
    pub(crate) struct Mutex<T: ?Sized> {
        state: AtomicU32,
        data: UnsafeCell<T>,
    }

    unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
    unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

    impl<T> Mutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self {
                state: AtomicU32::new(0),
                data: UnsafeCell::new(value),
            }
        }
    }

    impl<T: ?Sized> Mutex<T> {
        fn raw_lock(&self) {
            if self
                .state
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            for _ in 0..100 {
                if self.state.load(Ordering::Relaxed) == 0
                    && self
                        .state
                        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                {
                    return;
                }
                std::hint::spin_loop();
            }
            while self.state.swap(2, Ordering::Acquire) != 0 {
                futex_wait(&self.state, 2);
            }
        }

        fn raw_unlock(&self) {
            if self.state.swap(0, Ordering::Release) == 2 {
                futex_wake(&self.state, 1);
            }
        }

        /// Never poisoned (panics abort), so this always succeeds.
        pub(crate) fn lock(&self) -> Result<MutexGuard<'_, T>, Infallible> {
            self.raw_lock();
            Ok(MutexGuard { mutex: self })
        }
    }

    impl<T: ?Sized> std::fmt::Debug for Mutex<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Mutex")
        }
    }

    pub(crate) struct MutexGuard<'a, T: ?Sized> {
        mutex: &'a Mutex<T>,
    }

    impl<T: ?Sized> Deref for MutexGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            unsafe { &*self.mutex.data.get() }
        }
    }

    impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            unsafe { &mut *self.mutex.data.get() }
        }
    }

    impl<T: ?Sized> Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {
            self.mutex.raw_unlock();
        }
    }

    // ---- Condvar --------------------------------------------------------

    /// A sequence number waiters sleep on; a notify bumps it.
    pub(crate) struct Condvar {
        seq: AtomicU32,
    }

    impl Condvar {
        pub(crate) const fn new() -> Self {
            Self {
                seq: AtomicU32::new(0),
            }
        }

        pub(crate) fn wait<'a, T: ?Sized>(
            &self,
            guard: MutexGuard<'a, T>,
        ) -> Result<MutexGuard<'a, T>, Infallible> {
            // Read under the lock: a notify after the caller's last check
            // of its condition changes it, so the wait returns at once.
            let seq = self.seq.load(Ordering::Relaxed);
            let mutex = guard.mutex;
            drop(guard);
            futex_wait(&self.seq, seq);
            mutex.raw_lock();
            Ok(MutexGuard { mutex })
        }

        pub(crate) fn notify_one(&self) {
            self.seq.fetch_add(1, Ordering::Relaxed);
            futex_wake(&self.seq, 1);
        }

        pub(crate) fn notify_all(&self) {
            self.seq.fetch_add(1, Ordering::Relaxed);
            futex_wake(&self.seq, u32::MAX);
        }
    }

    impl std::fmt::Debug for Condvar {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Condvar")
        }
    }

    // ---- OnceLock (set once, read by other threads) ---------------------

    pub(crate) struct OnceLock<T> {
        /// 0 empty, 1 being set, 2 set.
        state: AtomicU32,
        value: UnsafeCell<MaybeUninit<T>>,
    }

    unsafe impl<T: Send> Send for OnceLock<T> {}
    unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}

    impl<T> OnceLock<T> {
        pub(crate) const fn new() -> Self {
            Self {
                state: AtomicU32::new(0),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            }
        }

        pub(crate) fn set(&self, value: T) -> Result<(), T> {
            if self
                .state
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                return Err(value);
            }
            unsafe { (*self.value.get()).write(value) };
            self.state.store(2, Ordering::Release);
            Ok(())
        }

        pub(crate) fn get(&self) -> Option<&T> {
            (self.state.load(Ordering::Acquire) == 2)
                .then(|| unsafe { (*self.value.get()).assume_init_ref() })
        }
    }

    impl<T> Drop for OnceLock<T> {
        fn drop(&mut self) {
            if *self.state.get_mut() == 2 {
                unsafe { self.value.get_mut().assume_init_drop() };
            }
        }
    }

    impl<T> std::fmt::Debug for OnceLock<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("OnceLock")
        }
    }

    // ---- Channels -------------------------------------------------------

    pub(crate) mod mpsc {
        use super::{Condvar, Mutex};
        use std::collections::VecDeque;
        use std::sync::Arc;
        pub(crate) use std::sync::mpsc::{RecvError, SendError, TryRecvError};

        struct State<T> {
            queue: VecDeque<T>,
            senders: usize,
            receiver: bool,
        }

        struct Chan<T> {
            state: Mutex<State<T>>,
            ready: Condvar,
        }

        pub(crate) struct Sender<T>(Arc<Chan<T>>);
        pub(crate) struct Receiver<T>(Arc<Chan<T>>);

        pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
            let chan = Arc::new(Chan {
                state: Mutex::new(State {
                    queue: VecDeque::new(),
                    senders: 1,
                    receiver: true,
                }),
                ready: Condvar::new(),
            });
            (Sender(Arc::clone(&chan)), Receiver(chan))
        }

        impl<T> Sender<T> {
            pub(crate) fn send(&self, value: T) -> Result<(), SendError<T>> {
                let mut s = self.0.state.lock().unwrap();
                if !s.receiver {
                    return Err(SendError(value));
                }
                s.queue.push_back(value);
                drop(s);
                self.0.ready.notify_one();
                Ok(())
            }
        }

        impl<T> Clone for Sender<T> {
            fn clone(&self) -> Self {
                self.0.state.lock().unwrap().senders += 1;
                Self(Arc::clone(&self.0))
            }
        }

        impl<T> Drop for Sender<T> {
            fn drop(&mut self) {
                let mut s = self.0.state.lock().unwrap();
                s.senders -= 1;
                let last = s.senders == 0;
                drop(s);
                if last {
                    self.0.ready.notify_all();
                }
            }
        }

        impl<T> Receiver<T> {
            pub(crate) fn recv(&self) -> Result<T, RecvError> {
                let mut s = self.0.state.lock().unwrap();
                loop {
                    if let Some(v) = s.queue.pop_front() {
                        return Ok(v);
                    }
                    if s.senders == 0 {
                        return Err(RecvError);
                    }
                    s = self.0.ready.wait(s).unwrap();
                }
            }

            pub(crate) fn try_recv(&self) -> Result<T, TryRecvError> {
                let mut s = self.0.state.lock().unwrap();
                match s.queue.pop_front() {
                    Some(v) => Ok(v),
                    None if s.senders == 0 => Err(TryRecvError::Disconnected),
                    None => Err(TryRecvError::Empty),
                }
            }
        }

        impl<T> Drop for Receiver<T> {
            fn drop(&mut self) {
                self.0.state.lock().unwrap().receiver = false;
            }
        }

        pub(crate) struct IntoIter<T>(Receiver<T>);

        impl<T> Iterator for IntoIter<T> {
            type Item = T;
            fn next(&mut self) -> Option<T> {
                self.0.recv().ok()
            }
        }

        impl<T> IntoIterator for Receiver<T> {
            type Item = T;
            type IntoIter = IntoIter<T>;
            fn into_iter(self) -> IntoIter<T> {
                IntoIter(self)
            }
        }
    }

    // ---- Worker threads ------------------------------------------------

    type Job = Box<dyn FnOnce() + Send>;

    /// Host workers, at most 64 (one bit each in `IDLE`).
    const MAX_WORKERS: usize = 64;

    struct Slot {
        /// 0 idle, 1 a job is waiting.
        state: AtomicU32,
        job: UnsafeCell<Option<Job>>,
    }

    // SAFETY: `job` is written only by the thread that claimed the slot
    // (before `state` becomes 1, Release) and taken only by the slot's
    // worker (after it sees 1, Acquire).
    unsafe impl Sync for Slot {}

    static SLOTS: [Slot; MAX_WORKERS] = [const {
        Slot {
            state: AtomicU32::new(0),
            job: UnsafeCell::new(None),
        }
    }; MAX_WORKERS];
    /// Bit `i`: worker `i` is waiting for a job.
    static IDLE: AtomicU64 = AtomicU64::new(0);
    /// Bumped whenever a worker becomes idle (for `claim_blocking`).
    static IDLE_SEQ: AtomicU32 = AtomicU32::new(0);

    fn mark_idle(index: usize) {
        IDLE.fetch_or(1 << index, Ordering::AcqRel);
        IDLE_SEQ.fetch_add(1, Ordering::Release);
        futex_wake(&IDLE_SEQ, u32::MAX);
    }

    fn try_claim() -> Option<usize> {
        let mut idle = IDLE.load(Ordering::Acquire);
        while idle != 0 {
            let i = idle.trailing_zeros() as usize;
            match IDLE.compare_exchange_weak(
                idle,
                idle & !(1 << i),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(i),
                Err(now) => idle = now,
            }
        }
        None
    }

    fn claim_blocking() -> usize {
        loop {
            let seq = IDLE_SEQ.load(Ordering::Acquire);
            if let Some(i) = try_claim() {
                return i;
            }
            futex_wait(&IDLE_SEQ, seq);
        }
    }

    fn dispatch(index: usize, job: Job) {
        let slot = &SLOTS[index];
        unsafe { *slot.job.get() = Some(job) };
        slot.state.store(1, Ordering::Release);
        futex_wake(&slot.state, 1);
    }

    /// The body of host worker `index` (its stack pointer already set by the
    /// host): run the jobs it is given, forever.
    pub fn thread_main(index: usize) {
        assert!(index < MAX_WORKERS, "at most 64 decoder threads");
        let slot = &SLOTS[index];
        loop {
            mark_idle(index);
            while slot.state.load(Ordering::Acquire) == 0 {
                futex_wait(&slot.state, 0);
            }
            let job = unsafe { (*slot.job.get()).take() }.expect("a dispatched job");
            job();
            slot.state.store(0, Ordering::Release);
        }
    }

    // ---- Thread identity -------------------------------------------------
    //
    // std's `thread_local!` is a plain static in this build (the prebuilt
    // std has no thread support), so state that must be per thread (the
    // command stream's level sink, `crate::recon`) is indexed by the stack
    // the code runs on: the host workers' stacks are registered when they
    // are allocated, everything else runs on the module's default stack.

    /// Thread slots: the default stack, then one per registered stack.
    pub(crate) const THREAD_SLOTS: usize = MAX_WORKERS + 1;
    static STACKS: [(AtomicU32, AtomicU32); MAX_WORKERS] =
        [const { (AtomicU32::new(0), AtomicU32::new(0)) }; MAX_WORKERS];
    static STACK_COUNT: AtomicU32 = AtomicU32::new(0);

    /// Register a host worker's stack, bytes `lo..hi` (before the worker
    /// runs anything), from one thread. At most 64; later ones are ignored.
    pub fn register_stack(lo: usize, hi: usize) {
        let i = STACK_COUNT.load(Ordering::Acquire) as usize;
        if i < MAX_WORKERS {
            STACKS[i].0.store(lo as u32, Ordering::Relaxed);
            STACKS[i].1.store(hi as u32, Ordering::Relaxed);
            STACK_COUNT.store(i as u32 + 1, Ordering::Release);
        }
    }

    /// This thread's slot: `1 + k` on registered stack `k`, 0 elsewhere
    /// (the default stack). Distinct for threads running at the same time.
    pub(crate) fn thread_slot() -> usize {
        let probe = 0u8;
        let at = std::hint::black_box(&probe) as *const u8 as usize as u32;
        let n = STACK_COUNT.load(Ordering::Acquire) as usize;
        STACKS[..n]
            .iter()
            .position(|(lo, hi)| {
                (lo.load(Ordering::Relaxed)..hi.load(Ordering::Relaxed)).contains(&at)
            })
            .map_or(0, |k| k + 1)
    }

    /// Workers waiting for a job now.
    pub fn idle_workers() -> u32 {
        IDLE.load(Ordering::Acquire).count_ones()
    }

    /// A task's result, and whether it has finished.
    struct Packet<T> {
        done: AtomicU32,
        value: UnsafeCell<Option<T>>,
    }

    unsafe impl<T: Send> Sync for Packet<T> {}

    impl<T> Packet<T> {
        fn new() -> Self {
            Self {
                done: AtomicU32::new(0),
                value: UnsafeCell::new(None),
            }
        }

        fn complete(&self, value: T) {
            unsafe { *self.value.get() = Some(value) };
            self.done.store(1, Ordering::Release);
            futex_wake(&self.done, u32::MAX);
        }

        fn wait(&self) -> T {
            while self.done.load(Ordering::Acquire) == 0 {
                futex_wait(&self.done, 0);
            }
            unsafe { (*self.value.get()).take() }.expect("a task result")
        }
    }

    // ---- Scoped tasks ---------------------------------------------------

    struct ScopeData {
        running: AtomicU32,
    }

    pub(crate) struct Scope<'scope, 'env: 'scope> {
        data: Arc<ScopeData>,
        scope: PhantomData<&'scope mut &'scope ()>,
        env: PhantomData<&'env mut &'env ()>,
    }

    pub(crate) struct ScopedJoinHandle<'scope, T> {
        packet: Arc<Packet<T>>,
        scope: PhantomData<&'scope ()>,
    }

    impl<T> ScopedJoinHandle<'_, T> {
        pub(crate) fn join(self) -> std::thread::Result<T> {
            Ok(self.packet.wait())
        }
    }

    pub(crate) fn scope<'env, F, T>(f: F) -> T
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
    {
        let s = Scope {
            data: Arc::new(ScopeData {
                running: AtomicU32::new(0),
            }),
            scope: PhantomData,
            env: PhantomData,
        };
        let result = f(&s);
        loop {
            let n = s.data.running.load(Ordering::Acquire);
            if n == 0 {
                break;
            }
            futex_wait(&s.data.running, n);
        }
        result
    }

    impl<'scope> Scope<'scope, '_> {
        /// Run `f` on an idle worker, or inline when none is idle.
        pub(crate) fn spawn<F, T>(&'scope self, f: F) -> ScopedJoinHandle<'scope, T>
        where
            F: FnOnce() -> T + Send + 'scope,
            T: Send + 'scope,
        {
            self.spawn_with(try_claim(), f)
        }

        fn spawn_with<F, T>(&'scope self, worker: Option<usize>, f: F) -> ScopedJoinHandle<'scope, T>
        where
            F: FnOnce() -> T + Send + 'scope,
            T: Send + 'scope,
        {
            let packet = Arc::new(Packet::new());
            let (p, data) = (Arc::clone(&packet), Arc::clone(&self.data));
            data.running.fetch_add(1, Ordering::AcqRel);
            let job = move || {
                p.complete(f());
                drop(p);
                if data.running.fetch_sub(1, Ordering::AcqRel) == 1 {
                    futex_wake(&data.running, u32::MAX);
                }
            };
            match worker {
                Some(i) => {
                    let job: Box<dyn FnOnce() + Send + 'scope> = Box::new(job);
                    // SAFETY: `scope` does not return before the job has
                    // finished (`running`), so what it borrows outlives it.
                    let job: Job = unsafe { std::mem::transmute(job) };
                    dispatch(i, job);
                }
                None => job(),
            }
            ScopedJoinHandle {
                packet,
                scope: PhantomData,
            }
        }
    }

    /// A worker reserved for one scoped task.
    pub(crate) struct Helper(usize);

    /// Reserve up to `n` idle workers for scoped tasks that wait on each
    /// other (fewer, possibly none, when fewer are idle).
    pub(crate) fn claim_helpers(n: usize) -> Vec<Helper> {
        (0..n).map_while(|_| try_claim().map(Helper)).collect()
    }

    pub(crate) fn spawn_on<'scope, 'env, T: Send + 'scope>(
        s: &'scope Scope<'scope, 'env>,
        helper: Option<Helper>,
        f: impl FnOnce() -> T + Send + 'scope,
    ) -> ScopedJoinHandle<'scope, T> {
        match helper {
            Some(Helper(i)) => s.spawn_with(Some(i), f),
            None => s.spawn(f),
        }
    }

    // ---- Long-lived threads --------------------------------------------

    pub(crate) struct JoinHandle(Arc<Packet<()>>);

    impl JoinHandle {
        pub(crate) fn join(self) -> std::thread::Result<()> {
            self.0.wait();
            Ok(())
        }
    }

    /// Occupy a worker for `f` (waiting for one to become idle).
    pub(crate) fn spawn_thread(_name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle {
        let packet = Arc::new(Packet::new());
        let p = Arc::clone(&packet);
        dispatch(
            claim_blocking(),
            Box::new(move || {
                f();
                p.complete(());
            }),
        );
        JoinHandle(packet)
    }
}

pub(crate) use imp::*;
