use tokio::sync::mpsc;

/// An impression event to be recorded after a won bid.
/// Drives the freq-cap INCR + EXPIRE Lua script in Redis.
#[derive(Debug, Clone)]
pub struct ImpressionEvent {
    pub user_id: String,
    pub campaign_id: u32,
    pub creative_id: u32,
    pub device_type: u8,
    pub hour_of_day: u8,
}

/// Off-bid-path write queue for impression events.
///
/// The bid path calls `try_record` (non-blocking). N worker tasks consume
/// from the channel and write freq-cap counters via Redis EVAL.
///
/// Bounded at 65_536. On overflow, `try_record` drops the event and increments
/// `bidder.freq_cap.recorder.dropped` — the bid path never blocks.
#[derive(Clone)]
pub struct ImpressionRecorder {
    tx: mpsc::Sender<ImpressionEvent>,
}

impl ImpressionRecorder {
    /// Create recorder + bounded channel. Workers are spawned by the caller
    /// via `spawn_workers` after Redis pool is available.
    pub fn new() -> (Self, mpsc::Receiver<ImpressionEvent>) {
        let (tx, rx) = mpsc::channel(65_536);
        (Self { tx }, rx)
    }

    /// Non-blocking enqueue. Drops silently on full channel.
    pub fn try_record(&self, event: ImpressionEvent) {
        match self.tx.try_send(event) {
            Ok(_) => {}
            Err(_) => {
                metrics::counter!("bidder.freq_cap.recorder.dropped").increment(1);
            }
        }
    }
}
