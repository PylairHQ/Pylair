//! Stream plumbing between the actor and a `call_stream` consumer: the
//! byte-leased chunk channel and its single terminal (protocol-v1.md
//! section 7, RC.8A stream buffer limits).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::handle::stopped_err;
use crate::protocol::ErrorObj;

/// A streamed item delivered to the caller of `call_stream`.
#[derive(Debug)]
pub enum StreamItem {
    Chunk {
        seq: u64,
        value: Value,
    },
    /// Terminal message for the stream: Ok(result) or Err(error).
    End(Result<Value, ErrorObj>),
}

/// Byte-budget lease for one buffered stream chunk (RC.8A): lives in the
/// channel item, so the counters it meters are released exactly when the
/// chunk leaves the buffer — recv'd by the consumer or dropped with the
/// channel. A consumer holding received items is draining, not stalling,
/// so its memory is not counted here.
struct StreamLease {
    per_stream: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    bytes: u64,
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.per_stream.fetch_sub(self.bytes, Ordering::Relaxed);
        self.total.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// Channel payload: the public `StreamItem::Chunk` plus its byte lease.
pub(super) struct WeightedChunk {
    seq: u64,
    value: Value,
    _lease: StreamLease,
}

/// Supervisor-side end of a stream. Chunks are bounded by count AND
/// bytes (`try_send`); the terminal result travels on its own one-shot,
/// so it can never be lost to a full chunk buffer and never awaits.
pub(super) struct StreamSink {
    chunks: mpsc::Sender<WeightedChunk>,
    end: Option<oneshot::Sender<Result<Value, ErrorObj>>>,
    per_stream: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    per_cap: u64,
    total_cap: u64,
}

/// Why a chunk could not be delivered.
#[derive(Debug)]
pub(super) enum ChunkFail {
    /// Chunk-count buffer full: the consumer is not draining.
    Stalled,
    /// Per-stream byte cap exceeded (RC.8A).
    StreamBytes,
    /// All-streams byte cap exceeded (RC.8A).
    TotalBytes,
    /// Receiver dropped: the consumer is gone.
    Gone,
}

impl StreamSink {
    pub(super) fn chunk(&self, seq: u64, value: Value) -> Result<(), ChunkFail> {
        // Serialized-size estimate — Value::to_string is already a
        // compact JSON encoding, close to the on-wire frame weight.
        let bytes = value.to_string().len() as u64;
        let per = self.per_stream.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if per > self.per_cap {
            self.per_stream.fetch_sub(bytes, Ordering::Relaxed);
            return Err(ChunkFail::StreamBytes);
        }
        let total = self.total.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if total > self.total_cap {
            self.total.fetch_sub(bytes, Ordering::Relaxed);
            self.per_stream.fetch_sub(bytes, Ordering::Relaxed);
            return Err(ChunkFail::TotalBytes);
        }
        self.chunks
            .try_send(WeightedChunk {
                seq,
                value,
                _lease: StreamLease {
                    per_stream: self.per_stream.clone(),
                    total: self.total.clone(),
                    bytes,
                },
            })
            .map_err(|e| match e {
                // On failure the rejected WeightedChunk (with its lease)
                // is handed back and dropped — counters release.
                mpsc::error::TrySendError::Full(_) => ChunkFail::Stalled,
                mpsc::error::TrySendError::Closed(_) => ChunkFail::Gone,
            })
    }

    /// Deliver the terminal. Never blocks; dropping the sink afterwards closes
    /// the chunk channel, which is how the receiver learns to read the end.
    pub(super) fn finish(mut self, result: Result<Value, ErrorObj>) {
        if let Some(end) = self.end.take() {
            let _ = end.send(result);
        }
    }
}

/// Consumer-side end of a stream: queued chunks first, then exactly one
/// `StreamItem::End`, then `None`.
pub struct StreamRx {
    pub(super) chunks: mpsc::Receiver<WeightedChunk>,
    end: Option<oneshot::Receiver<Result<Value, ErrorObj>>>,
}

impl StreamRx {
    pub async fn recv(&mut self) -> Option<StreamItem> {
        if self.end.is_some() {
            if let Some(c) = self.chunks.recv().await {
                return Some(StreamItem::Chunk {
                    seq: c.seq,
                    value: c.value,
                });
            }
            // Chunk channel closed => the sink is gone and the terminal is set
            // (or the supervisor itself dropped).
            let end = self.end.take()?;
            return Some(StreamItem::End(
                end.await.unwrap_or_else(|_| Err(stopped_err())),
            ));
        }
        None
    }
}

pub(super) fn stream_pair(
    buffer: usize,
    per_stream_cap: u64,
    total: Arc<AtomicU64>,
    total_cap: u64,
) -> (StreamSink, StreamRx) {
    let (ctx, crx) = mpsc::channel(buffer.max(1));
    let (etx, erx) = oneshot::channel();
    (
        StreamSink {
            chunks: ctx,
            end: Some(etx),
            per_stream: Arc::new(AtomicU64::new(0)),
            total,
            per_cap: per_stream_cap,
            total_cap,
        },
        StreamRx {
            chunks: crx,
            end: Some(erx),
        },
    )
}
