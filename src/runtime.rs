use std::{
    future::Future,
    sync::{Once, OnceLock},
    thread,
};

use async_executor::{Executor, Task};

static EXECUTOR: OnceLock<&'static Executor<'static>> = OnceLock::new();
static WORKERS_STARTED: Once = Once::new();

pub(crate) fn block_on<T>(future: impl Future<Output = T>) -> T {
    let executor = executor();
    WORKERS_STARTED.call_once(|| start_workers(executor));

    async_io::block_on(executor.run(future))
}

pub(crate) fn spawn<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> Task<T> {
    executor().spawn(future)
}

pub(crate) fn detach<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) {
    spawn(future).detach();
}

pub(crate) async fn unblock<T: Send + 'static>(function: impl FnOnce() -> T + Send + 'static) -> T {
    blocking::unblock(function).await
}

fn start_workers(executor: &'static Executor<'static>) {
    for index in 0..worker_count().saturating_sub(1) {
        let worker = executor;
        let name = format!("pocketboot-async-{index}");
        match thread::Builder::new()
            .name(name.clone())
            .spawn(move || async_io::block_on(worker.run(futures_lite::future::pending::<()>())))
        {
            Ok(_thread) => tracing::info!(thread = name, "async worker thread spawned"),
            Err(err) => {
                tracing::warn!(thread = name, error = ?err, "failed to spawn async worker thread")
            }
        }
    }
}

fn executor() -> &'static Executor<'static> {
    *EXECUTOR.get_or_init(|| Box::leak(Box::new(Executor::new())))
}

fn worker_count() -> usize {
    thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .max(1)
}
