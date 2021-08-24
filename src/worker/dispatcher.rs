use crate::{pollers::GatewayRef, worker::Worker, WorkerConfig, WorkerRegistrationError};
use arc_swap::ArcSwap;
use futures::future::join_all;
use std::{collections::HashMap, ops::Deref, sync::Arc};
use tokio::sync::Notify;

/// Allows access to workers by task queue name
#[derive(Default)]
pub struct WorkerDispatcher {
    /// Maps task queue names to workers
    workers: ArcSwap<HashMap<String, WorkerRefCt>>,
}

impl WorkerDispatcher {
    pub async fn new_worker(
        &self,
        config: WorkerConfig,
        sticky_queue: Option<String>,
        gateway: Arc<GatewayRef>,
    ) -> Result<(), WorkerRegistrationError> {
        let tq = config.task_queue.clone();
        let worker = Worker::new(config, sticky_queue, gateway);
        self.set_worker_for_task_queue(tq, worker)
    }

    pub fn set_worker_for_task_queue(
        &self,
        tq: String,
        worker: Worker,
    ) -> Result<(), WorkerRegistrationError> {
        if self.workers.load().get(&tq).is_some() {
            return Err(WorkerRegistrationError::WorkerAlreadyRegisteredForQueue(tq));
        }
        let tq = &tq;
        let worker = WorkerRefCt::new(worker);
        self.workers.rcu(|map| {
            let mut map = HashMap::clone(map);
            map.insert(tq.clone(), worker.clone());
            map
        });
        Ok(())
    }

    pub fn get(&self, task_queue: &str) -> Option<impl Deref<Target = Worker>> {
        self.workers.load().get(task_queue).cloned()
    }

    pub async fn shutdown_one(&self, task_queue: &str) {
        info!("Shutting down worker on queue {}", task_queue);
        let mut maybe_worker = None;
        if let Some(w) = self.workers.load().get(task_queue) {
            w.shutdown().await;
            self.workers.rcu(|map| {
                let mut map = HashMap::clone(map);
                if maybe_worker.is_none() {
                    maybe_worker = map.remove(task_queue);
                }
                map
            });
        }
        if let Some(w) = maybe_worker {
            w.destroy().await;
        }
    }

    pub async fn shutdown_all(&self) {
        // First notify all workers and allow tasks to drain
        join_all(self.workers.load().values().map(|w| w.shutdown())).await;

        let mut all_workers = HashMap::new();
        self.workers.rcu(|map| {
            let mut map = HashMap::clone(map);
            all_workers.extend(map.drain());
            map
        });
        join_all(all_workers.into_values().map(|w| w.destroy())).await;
    }
}

/// Fun little struct that allows us to efficiently `await` for outstanding references to workers
/// to reach 0 before we consume it forever.
#[derive(Clone)]
struct WorkerRefCt {
    inner: Option<Arc<Worker>>,
    notify: Arc<Notify>,
}

impl WorkerRefCt {
    fn new(worker: Worker) -> Self {
        Self {
            inner: Some(Arc::new(worker)),
            notify: Arc::new(Notify::new()),
        }
    }

    async fn destroy(mut self) {
        let mut arc = self.inner.take().unwrap();
        loop {
            self.notify.notified().await;
            match Arc::try_unwrap(arc) {
                Ok(w) => {
                    w.finalize_shutdown().await;
                    return;
                }
                Err(a) => {
                    arc = a;
                    continue;
                }
            }
        }
    }
}

impl Deref for WorkerRefCt {
    type Target = Worker;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("Must exist").deref()
    }
}

impl Drop for WorkerRefCt {
    fn drop(&mut self) {
        match &self.inner {
            // Notify once destroy has been requested
            None => self.notify.notify_one(),
            Some(arc) => {
                // We wait until 2 rather than 1 because we ourselves still have an Arc
                if Arc::strong_count(arc) == 2 {
                    self.notify.notify_one()
                }
            }
        };
    }
}