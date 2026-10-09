//! A shared, fail-fast budget for all HTTP password hashing and verification.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Semaphore;

// Argon2's default needs 19 MiB per job. Keep room for normal wallet work
// inside a 256 MiB service. There is no application waiting queue.
const WORKERS: usize = 2;

pub(crate) struct PasswordWork {
    slots: Arc<Semaphore>,
    rejected: AtomicU64,
}

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
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, WorkError> {
        let slot = self.slots.clone().try_acquire_owned().map_err(|_| {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            WorkError::Busy
        })?;
        tokio::task::spawn_blocking(move || {
            // A disconnected caller cannot release capacity while Argon2 still runs.
            let _slot = slot;
            work()
        })
        .await
        .map_err(|_| WorkError::Failed)
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
        assert_eq!(
            work.run(|| panic!("must not run when full")).await,
            Err(WorkError::Busy)
        );
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
        assert_eq!(work.run(|| 42).await, Ok(42));
    }

    #[tokio::test]
    async fn failed_worker_releases_its_slot() {
        let work = PasswordWork::default();
        assert_eq!(work.run(|| panic!("test worker failure")).await, Err(WorkError::Failed));
        assert_eq!(work.slots.available_permits(), WORKERS);
        assert_eq!(work.run(|| 42).await, Ok(42));
    }
}
