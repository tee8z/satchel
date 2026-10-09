//! A shared, fail-fast budget for all HTTP password hashing and verification.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// Each job allocates Argon2's working memory: 19 MiB with the default
// parameters this service hashes with (`argon2::Params::DEFAULT_M_COST` KiB).
// Two workers cap that at 38 MiB however many requests arrive. There is no
// application waiting queue.
const WORKERS: usize = 2;

pub(crate) struct PasswordWork {
    slots: Arc<Semaphore>,
    rejected: AtomicU64,
}

/// A worker reserved for one password job. Handlers take it before they spend
/// a rate-limit attempt or a proof of work, so a busy refusal costs nothing;
/// dropping it unused frees the worker.
pub(crate) struct PasswordPermit(OwnedSemaphorePermit);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkError {
    Busy,
    Failed,
}

impl Default for PasswordWork {
    fn default() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(WORKERS)),
            rejected: AtomicU64::new(0),
        }
    }
}

impl PasswordWork {
    /// Reserves a worker without waiting, or refuses with `Busy`.
    pub(crate) fn admit(&self) -> Result<PasswordPermit, WorkError> {
        self.slots.clone().try_acquire_owned().map(PasswordPermit).map_err(|_| {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            WorkError::Busy
        })
    }

    pub(crate) fn metrics(&self) -> String {
        format!(
            "# HELP satchel_password_jobs Password jobs admitted, including blocking work whose caller disconnected.\n\
             # TYPE satchel_password_jobs gauge\nsatchel_password_jobs {}\n\
             # HELP satchel_password_jobs_rejected_total Password jobs refused because both workers were occupied.\n\
             # TYPE satchel_password_jobs_rejected_total counter\nsatchel_password_jobs_rejected_total {}\n",
            WORKERS - self.slots.available_permits(),
            self.rejected.load(Ordering::Relaxed),
        )
    }
}

impl PasswordPermit {
    pub(crate) async fn run<T: Send + 'static>(
        self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, WorkError> {
        tokio::task::spawn_blocking(move || {
            // A disconnected caller cannot release capacity while Argon2 still runs.
            let _slot = self.0;
            work()
        })
        .await
        .map_err(|_| WorkError::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn cancellation_keeps_capacity_until_blocking_work_finishes() {
        let work = Arc::new(PasswordWork::default());
        let mut releases = vec![];
        let mut callers = vec![];
        for _ in 0..WORKERS {
            let (release, wait) = mpsc::channel();
            let (started, ready) = oneshot::channel();
            let worker = work.clone();
            callers.push(tokio::spawn(async move {
                worker
                    .admit()
                    .unwrap()
                    .run(move || {
                        started.send(()).unwrap();
                        let _ = wait.recv();
                    })
                    .await
            }));
            tokio::time::timeout(Duration::from_secs(5), ready)
                .await
                .unwrap()
                .unwrap();
            releases.push(release);
        }
        for caller in callers {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
        }
        assert_eq!(work.admit().err(), Some(WorkError::Busy));
        assert!(work.metrics().contains("satchel_password_jobs 2\n"));
        assert!(work.metrics().contains("satchel_password_jobs_rejected_total 1\n"));
        for release in releases {
            release.send(()).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while work.slots.available_permits() != WORKERS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(work.admit().unwrap().run(|| 42).await, Ok(42));
    }

    #[test]
    fn a_permit_dropped_without_work_frees_its_worker() {
        let work = PasswordWork::default();
        let permits: Vec<_> = (0..WORKERS).map(|_| work.admit().unwrap()).collect();
        assert_eq!(work.admit().err(), Some(WorkError::Busy));
        drop(permits);
        assert!(work.admit().is_ok());
        assert!(work.metrics().contains("satchel_password_jobs_rejected_total 1\n"));
    }

    #[tokio::test]
    async fn failed_worker_releases_its_slot() {
        let work = PasswordWork::default();
        assert_eq!(
            work.admit().unwrap().run(|| panic!("test worker failure")).await,
            Err(WorkError::Failed)
        );
        assert_eq!(work.slots.available_permits(), WORKERS);
        assert_eq!(work.admit().unwrap().run(|| 42).await, Ok(42));
    }
}
