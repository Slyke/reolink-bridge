use std::{
    future::Future,
    io::ErrorKind,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use crate::diagnostics::Diagnostics;
use anyhow::Result;
use serde_json::json;

pub const DEFAULT: u8 = 2;
pub const MAX: u8 = 10;

#[derive(Clone)]
pub struct ReadRetries {
    limit: Arc<AtomicU8>,
    diagnostics: Option<Diagnostics>,
    camera: Option<Arc<str>>,
}

impl Default for ReadRetries {
    fn default() -> Self {
        Self {
            limit: Arc::new(AtomicU8::new(DEFAULT)),
            diagnostics: None,
            camera: None,
        }
    }
}

impl ReadRetries {
    pub fn new(limit: u8, diagnostics: Diagnostics) -> Result<Self> {
        anyhow::ensure!(
            limit <= MAX,
            "api.cameraReadRetries must be between 0 and {MAX}"
        );
        Ok(Self {
            limit: Arc::new(AtomicU8::new(limit)),
            diagnostics: Some(diagnostics),
            camera: None,
        })
    }

    pub fn for_camera(&self, camera: &str) -> Self {
        Self {
            camera: Some(Arc::from(camera)),
            ..self.clone()
        }
    }

    pub fn get(&self) -> u8 {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn set(&self, limit: u8) -> bool {
        if limit > MAX {
            return false;
        }
        self.limit.store(limit, Ordering::Relaxed);
        true
    }

    // Opt in at the camera operation boundary, where we know a read can be restarted
    // from a fresh session without duplicating a physical action or publishing partial data.
    pub async fn read<T, F: Future<Output = Result<T>>>(
        &self,
        name: &str,
        operation: impl FnMut() -> F,
    ) -> Result<T> {
        self.read_if(name, operation, transient).await
    }

    pub(crate) async fn read_if<T, E, F: Future<Output = std::result::Result<T, E>>>(
        &self,
        name: &str,
        mut operation: impl FnMut() -> F,
        retryable: impl Fn(&E) -> bool,
    ) -> std::result::Result<T, E> {
        let limit = self.get();
        for retry in 0..limit {
            match operation().await {
                Err(error) if retryable(&error) => {
                    if let Some(diagnostics) = &self.diagnostics {
                        diagnostics.log("reolink::retry", "CAMERA_READ_RETRY", "warn", None, json!({
                            "cameraId": self.camera.as_deref(), "operation":name, "retry":retry + 1, "retryLimit":limit
                        }));
                    }
                    tokio::time::sleep(Duration::from_millis(200 << retry.min(3))).await;
                }
                result => return result,
            }
        }
        operation().await
    }
}

pub(crate) fn transient(error: &anyhow::Error) -> bool {
    error.is::<tokio::time::error::Elapsed>()
        || error.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::TimedOut
                    | ErrorKind::ConnectionRefused
                    | ErrorKind::NotConnected
                    | ErrorKind::Interrupted
                    | ErrorKind::WouldBlock
            )
        })
}
