//! Bounded CPU-to-viewport handoff. The worker retains one live simulation;
//! pausing applies backpressure instead of accumulating a whole animation.

use crate::fx::{self, prepare::PreparedPlayback, simulation::StreamFrame};
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};

pub struct Playback {
    pub prepared: Arc<PreparedPlayback>,
    receiver: Receiver<Result<StreamFrame, String>>,
    cancelled: Arc<AtomicBool>,
}

impl Playback {
    pub fn start(prepared: Arc<PreparedPlayback>) -> Self {
        // Two queued frames plus the frame being displayed keep both memory
        // and preparation latency independent of the effect's total lifetime.
        let (sender, receiver) = mpsc::sync_channel(2);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let input = prepared.clone();
        std::thread::spawn(move || {
            let mut on_frame = |mut packet: StreamFrame| -> Result<bool, String> {
                if stop.load(Ordering::Acquire) {
                    return Ok(false);
                }
                input.projection.project(&mut packet.frame)?;
                fx::compact_frame(&mut packet.frame);
                packet.frame["background"] = json!(0xff191e23_u32);
                if fx::memory_cost(&packet.frame) > fx::prepare::MAX_CLIP_BYTES / 4 {
                    return Err("This effect exceeds the live frame memory limit".into());
                }
                Ok(sender.send(Ok(packet)).is_ok() && !stop.load(Ordering::Acquire))
            };
            let result = fx::simulation::stream(&input.source, 17, 33, &mut on_frame);
            if let Err(error) = result {
                if !stop.load(Ordering::Acquire) {
                    // Simulation/projection guards describe an unsupported
                    // effect, unlike a temporarily unavailable graphics device.
                    let _ = sender.send(Err(format!("unsupported: {error}")));
                }
            }
        });
        Self {
            prepared,
            receiver,
            cancelled,
        }
    }

    /// Only startup waits for a frame, and the caller does that off the UI
    /// thread. Normal drawing always uses the non-blocking receive below.
    pub fn first(&self) -> Result<StreamFrame, String> {
        self.receiver
            .recv()
            .map_err(|_| "The effect worker stopped before its first frame".to_owned())?
    }

    pub fn next(&self) -> Result<Option<StreamFrame>, String> {
        match self.receiver.try_recv() {
            Ok(packet) => packet.map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("The effect worker stopped unexpectedly".into()),
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        // Dropping the receiver also releases a worker blocked on a full queue.
        // There is no join on the UI thread and no detached queue of snapshots.
    }
}
