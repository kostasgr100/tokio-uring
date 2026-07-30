//! IORING_OP_FUTEX_WAIT — park on a futex word without leaving the uring
//! runtime.
//!
//! WHY THIS EXISTS (headless-sniper): the fire path fans a signal out to 10
//! worker threads. Doing that as 10 sequential channel sends costs 10 separate
//! wakes, and on a KVM guest each wake is a reschedule IPI and therefore a VM
//! exit — measured at 15-22us apiece, ~223us for the full fan-out, with late
//! positions paying the whole ramp.
//!
//! One `futex_wake(nr = INT_MAX)` releases every waiter from a single syscall.
//! But the workers cannot simply block on `futex(2)`: each drives its own
//! tokio-uring runtime, and blocking the thread would freeze the TP watchers
//! and fill polls running as spawned tasks on it. Submitting the wait as an SQE
//! keeps the executor live while the thread parks.
//!
//! Requires kernel 6.7+ (IORING_OP_FUTEX_WAIT). Confirm at runtime with
//! IORING_REGISTER_PROBE rather than a version check — an unsupported opcode
//! fails with -EINVAL rather than anything obvious.

use crate::runtime::driver::op::{Completable, CqeResult, Op};
use crate::runtime::CONTEXT;
use std::io;

/// futex2(2) modifier flags. From linux/futex.h:
///   FUTEX2_SIZE_U32 = 0x02  — the futex word is a u32
///   FUTEX2_PRIVATE  = 0x80  — process-private (skips the mmap-sem/inode path)
pub const FUTEX2_SIZE_U32: u32 = 0x02;
pub const FUTEX2_PRIVATE: u32 = 0x80;

/// Wake/wait on any bit. Truncated to futex bits by the kernel.
pub const FUTEX_BITSET_MATCH_ANY: u64 = 0xffff_ffff;

pub struct FutexWait {}

impl Op<FutexWait> {
    /// Park until `*futex != val`.
    ///
    /// Semantics match FUTEX_WAIT: the kernel re-checks the word under the
    /// futex bucket lock and returns EAGAIN *immediately* if it already differs
    /// from `val`. That check is what makes the lost-wakeup race impossible —
    /// a waker that bumps the word between our load and our submit cannot
    /// leave us parked.
    ///
    /// # Safety
    /// `futex` must point at a u32 that stays valid and mapped for as long as
    /// the operation is in flight. A `&'static AtomicU32` satisfies this; a
    /// stack local does not.
    pub unsafe fn futex_wait(
        futex: *const u32,
        val: u64,
        mask: u64,
        futex_flags: u32,
    ) -> io::Result<Op<FutexWait>> {
        use io_uring::opcode;

        CONTEXT.with(|x| {
            x.handle()
                .expect("Not in a runtime context")
                .submit_op(FutexWait {}, |_| {
                    opcode::FutexWait::new(futex, val, mask, futex_flags).build()
                })
        })
    }
}

impl Completable for FutexWait {
    type Output = io::Result<()>;

    fn complete(self, cqe: CqeResult) -> Self::Output {
        cqe.result.map(|_| ())
    }
}

#[cfg(test)]
mod test {
    use crate as tokio_uring;
    use std::sync::atomic::{AtomicU32, Ordering};

    static WORD: AtomicU32 = AtomicU32::new(0);

    /// EAGAIN path: the word already differs, so the wait must return at once
    /// rather than park. This is the case that protects against lost wakeups.
    #[test]
    fn futex_wait_returns_eagain_when_value_differs() {
        WORD.store(7, Ordering::SeqCst);
        tokio_uring::start(async {
            let res = tokio_uring::futex_wait(&WORD, 0).await;
            match res {
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {}
                other => panic!("expected EAGAIN, got {:?}", other),
            }
        });
    }

    /// Wake path: park, have another thread bump the word and wake, and make
    /// sure we come back.
    #[test]
    fn futex_wait_is_woken_by_futex_wake() {
        WORD.store(0, Ordering::SeqCst);
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            WORD.store(1, Ordering::SeqCst);
            tokio_uring::futex_wake_all(&WORD);
        });
        tokio_uring::start(async {
            tokio_uring::futex_wait(&WORD, 0).await.unwrap();
        });
        assert_eq!(WORD.load(Ordering::SeqCst), 1);
    }
}
