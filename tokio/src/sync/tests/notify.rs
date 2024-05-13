use crate::sync::{self, Notify};
use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::task::{Context, RawWaker, RawWakerVTable, Waker};

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
use wasm_bindgen_test::wasm_bindgen_test as test;

#[test]
fn notify_clones_waker_before_lock() {
    const VTABLE: &RawWakerVTable = &RawWakerVTable::new(clone_w, wake, wake_by_ref, drop_w);

    unsafe fn clone_w(data: *const ()) -> RawWaker {
        let ptr = data as *const Notify;
        Arc::<Notify>::increment_strong_count(ptr);
        // Or some other arbitrary code that shouldn't be executed while the
        // Notify wait list is locked.
        (*ptr).notify_one();
        RawWaker::new(data, VTABLE)
    }

    unsafe fn drop_w(data: *const ()) {
        drop(Arc::<Notify>::from_raw(data as *const Notify));
    }

    unsafe fn wake(_data: *const ()) {
        unreachable!()
    }

    unsafe fn wake_by_ref(_data: *const ()) {
        unreachable!()
    }

    let notify = Arc::new(Notify::new());
    let notify2 = notify.clone();

    let waker =
        unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(notify2) as *const _, VTABLE)) };
    let mut cx = Context::from_waker(&waker);

    let future = notify.notified();
    pin!(future);

    // The result doesn't matter, we're just testing that we don't deadlock.
    let _ = future.poll(&mut cx);
}

#[cfg(panic = "unwind")]
#[test]
fn notify_waiters_handles_panicking_waker() {
    use futures::task::ArcWake;

    let notify = Arc::new(Notify::new());

    struct PanickingWaker(#[allow(dead_code)] Arc<Notify>);

    impl ArcWake for PanickingWaker {
        fn wake_by_ref(_arc_self: &Arc<Self>) {
            panic!("waker panicked");
        }
    }

    let bad_fut = notify.notified();
    pin!(bad_fut);

    let waker = futures::task::waker(Arc::new(PanickingWaker(notify.clone())));
    let mut cx = Context::from_waker(&waker);
    let _ = bad_fut.poll(&mut cx);

    let mut futs = Vec::new();
    for _ in 0..32 {
        let mut fut = tokio_test::task::spawn(notify.notified());
        assert!(fut.poll().is_pending());
        futs.push(fut);
    }

    assert!(std::panic::catch_unwind(|| {
        notify.notify_waiters();
    })
    .is_err());

    for mut fut in futs {
        assert!(fut.poll().is_ready());
    }
}

#[test]
fn notify_simple() {
    let notify = Notify::new();

    let mut fut1 = tokio_test::task::spawn(notify.notified());
    assert!(fut1.poll().is_pending());

    let mut fut2 = tokio_test::task::spawn(notify.notified());
    assert!(fut2.poll().is_pending());

    notify.notify_waiters();

    assert!(fut1.poll().is_ready());
    assert!(fut2.poll().is_ready());
}

#[test]
#[cfg(not(target_family = "wasm"))]
fn watch_test() {
    let rt = crate::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        let (tx, mut rx) = crate::sync::watch::channel(());

        crate::spawn(async move {
            let _ = tx.send(());
        });

        let _ = rx.changed().await;
    });
}

struct AssertDropHandle {
    is_dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl AssertDropHandle {
    #[track_caller]
    fn assert_dropped(&self) {
        assert!(self.is_dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[track_caller]
    fn assert_not_dropped(&self) {
        assert!(!self.is_dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}

struct AssertDrop {
    is_dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl AssertDrop {
    fn new() -> (Self, AssertDropHandle) {
        let shared = Arc::new(std::sync::atomic::AtomicBool::new(false));
        (
            AssertDrop {
                is_dropped: shared.clone(),
            },
            AssertDropHandle {
                is_dropped: shared.clone(),
            },
        )
    }
}
impl Drop for AssertDrop {
    fn drop(&mut self) {
        self.is_dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn keke() {
    use crate as tokio;
    use std::time::Duration;
    use tokio::runtime::Handle;
    use tokio::sync::mpsc::channel;
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let (tx, mut rx) = channel::<JoinHandle<AssertDrop>>(1);

        let (a_tx, mut a_rx) = channel::<AssertDropHandle>(1);
        let a = tokio::spawn(async move {
            let (a_ad, a_handle) = AssertDrop::new();
            a_tx.send(a_handle).await.unwrap();

            let b = rx.recv().await.unwrap();
            // let _ = b.await;

            if timeout(Duration::from_secs(3), b).await.is_err() {
                // Dropping handle b
                println!("First task timeout elapsed");
            } else {
                // b completed
                println!("First task handle awaited complete");
            }
            println!("a finished, dropping b handle");

            a_ad
        });

        let (b_tx, mut b_rx) = channel::<AssertDropHandle>(1);
        let b = tokio::spawn(async move {
            let (b_ad, b_handle) = AssertDrop::new();
            b_tx.send(b_handle).await.unwrap();

            // tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = a.await;

            println!("b finished, dropping a handle");
            b_ad
        });

        tx.send(b).await.unwrap();
        let b_handle = b_rx.recv().await.unwrap();
        let a_handle = a_rx.recv().await.unwrap();

        sleep(Duration::from_secs(5)).await;

        a_handle.assert_dropped();
        b_handle.assert_dropped();
    });

    // assert!(false);
}

use futures::FutureExt;
use std::pin::Pin;
use std::task::Poll;

struct MyFuture<T: Future + 'static> {
    fut: std::pin::Pin<Box<T>>,
}

impl<T> Future for MyFuture<T>
where
    T: Future + 'static,
    T::Output: 'static,
{
    type Output = T::Output;
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.fut.poll_unpin(cx)
    }
}

impl<T: Future + 'static> Drop for MyFuture<T> {
    fn drop(&mut self) {
        println!("Dropping MyFuture");
    }
}

#[test]
fn kaka() {
    use crate as tokio;
    use std::time::Duration;
    use tokio::sync::mpsc::channel;
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let (tx, mut rx) = channel::<JoinHandle<AssertDrop>>(1);

        let (a_tx, mut a_rx) = channel::<AssertDropHandle>(1);
        let a = tokio::spawn(MyFuture {
            fut: Box::pin(async move {
                let (a_ad, a_handle) = AssertDrop::new();
                a_tx.send(a_handle).await.unwrap();

                let b = rx.recv().await.unwrap();
                // let _ = b.await;

                if timeout(Duration::from_secs(3), b).await.is_err() {
                    // Dropping handle b
                    println!("First task timeout elapsed");
                } else {
                    // b completed
                    println!("First task handle awaited complete");
                }
                println!("a finished, dropping b handle");

                a_ad
            }),
        });

        let (b_tx, mut b_rx) = channel::<AssertDropHandle>(1);
        let b = tokio::spawn(MyFuture {
            fut: Box::pin(async move {
                let (b_ad, b_handle) = AssertDrop::new();
                b_tx.send(b_handle).await.unwrap();

                // tokio::time::sleep(Duration::from_secs(5)).await;
                let _ = a.await;

                println!("b finished, dropping a handle");
                b_ad
            }),
        });

        tx.send(b).await.unwrap();
        let b_handle = b_rx.recv().await.unwrap();
        let a_handle = a_rx.recv().await.unwrap();

        sleep(Duration::from_secs(5)).await;

        a_handle.assert_dropped();
        b_handle.assert_dropped();
    });

    // assert!(false);
}
