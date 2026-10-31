//! Minimal persistent async worker.
//!
//! A worker is one long-lived Tokio task that repeatedly takes an item from a
//! queue and awaits the caller's handler. Cancellation is a `watch` flag; the
//! task's `JoinHandle` is the completion boundary. No scheduler or worker-trait
//! layer exists here on purpose; later milestones reuse this directly.
#![allow(dead_code)]

use std::future::Future;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub struct Worker {
    cancel: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

/// Spawn one persistent worker for `queue`.
pub fn spawn<T, F, Fut>(mut queue: mpsc::Receiver<T>, mut handler: F) -> Worker
where
    T: Send + 'static,
    F: FnMut(T) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (cancel, mut cancelled) = watch::channel(false);
    let handle = tokio::spawn(async move {
        loop {
            let item = tokio::select! {
                biased;
                _ = cancelled.changed() => break,
                item = queue.recv() => item,
            };
            let Some(item) = item else {
                break;
            };
            tokio::select! {
                biased;
                _ = cancelled.changed() => break,
                _ = handler(item) => {}
            }
        }
    });
    Worker { cancel, handle }
}

impl Worker {
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }

    /// Cancel any in-flight handler and wait for the task to finish.
    pub async fn shutdown(self) {
        self.cancel();
        let _ = self.handle.await;
    }

    /// Wait for the task to finish after the queue closes.
    pub async fn wait(self) {
        let _ = self.handle.await;
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Barrier, oneshot};

    use super::*;

    #[tokio::test]
    async fn one_worker_processes_items_sequentially() {
        let (items_tx, items_rx) = mpsc::channel::<i32>(8);
        let (done_tx, mut done_rx) = mpsc::channel::<i32>(8);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let worker = spawn(items_rx, {
            let seen = Arc::clone(&seen);
            let done_tx = done_tx.clone();
            move |item| {
                let seen = Arc::clone(&seen);
                let done_tx = done_tx.clone();
                async move {
                    seen.lock().unwrap().push(item);
                    done_tx.send(item).await.unwrap();
                }
            }
        });
        items_tx.send(1).await.unwrap();
        items_tx.send(2).await.unwrap();
        drop(items_tx);
        worker.wait().await;
        assert_eq!(done_rx.recv().await, Some(1));
        assert_eq!(done_rx.recv().await, Some(2));
        assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
    }

    async fn connect_and_wait(addr: std::net::SocketAddr) -> usize {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(b"GET\n").await.unwrap();
        let mut buffer = [0u8; 8];
        let read = stream.read(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..read], b"ok\n");
        stream.shutdown().await.unwrap();
        read
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_workers_have_requests_in_flight_at_the_same_time() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let server = tokio::spawn({
            let barrier = Arc::clone(&barrier);
            async move {
                for _ in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let barrier = Arc::clone(&barrier);
                    tokio::spawn(async move {
                        let mut request = [0u8; 16];
                        let read = socket.read(&mut request).await.unwrap();
                        assert_eq!(&request[..read], b"GET\n");
                        // Returns only when a second request is also in flight.
                        barrier.wait().await;
                        socket.write_all(b"ok\n").await.unwrap();
                        socket.shutdown().await.unwrap();
                    });
                }
            }
        });
        let (items_a, queue_a) = mpsc::channel::<usize>(2);
        let (items_b, queue_b) = mpsc::channel::<usize>(2);
        let (done_tx, mut done_rx) = mpsc::channel::<usize>(4);
        let make_worker = |queue, done_tx: mpsc::Sender<usize>| {
            spawn(queue, move |item| {
                let done_tx = done_tx.clone();
                async move {
                    connect_and_wait(addr).await;
                    done_tx.send(item).await.unwrap();
                }
            })
        };
        let worker_a = make_worker(queue_a, done_tx.clone());
        let worker_b = make_worker(queue_b, done_tx.clone());
        items_a.send(1).await.unwrap();
        items_b.send(2).await.unwrap();
        let mut finished = Vec::new();
        for _ in 0..2 {
            finished.push(
                tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
                    .await
                    .expect("both workers must complete")
                    .expect("worker result"),
            );
        }
        finished.sort_unstable();
        assert_eq!(finished, vec![1, 2]);
        drop(items_a);
        drop(items_b);
        worker_a.wait().await;
        worker_b.wait().await;
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_waits_for_the_task_and_closes_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 16];
            let read = socket.read(&mut request).await.unwrap();
            assert_eq!(&request[..read], b"GET\n");
            accepted_tx.send(()).unwrap();
            let mut rest = Vec::new();
            let _ = socket.read_to_end(&mut rest).await;
            closed_tx.send(()).unwrap();
        });
        let (items_tx, items_rx) = mpsc::channel::<i32>(4);
        let worker = spawn(items_rx, move |_item| async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(b"GET\n").await.unwrap();
            let mut buffer = [0u8; 16];
            let _ = stream.read(&mut buffer).await.unwrap();
        });
        items_tx.send(1).await.unwrap();
        accepted_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), worker.shutdown())
            .await
            .expect("shutdown must finish");
        tokio::time::timeout(Duration::from_secs(3), closed_rx)
            .await
            .expect("connection must close")
            .expect("server signal");
        server.await.unwrap();
        drop(items_tx);
    }
}
