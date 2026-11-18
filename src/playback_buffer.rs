use super::Frame;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};
use tokio::sync::Notify;

const MAX_FRAMES: usize = 8;
const MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
pub struct PlaybackBuffer {
    queue: Mutex<Queue>,
    space: Notify,
    source_ready: AtomicBool,
}

#[derive(Default)]
struct Queue {
    frames: VecDeque<(f64, Frame)>,
    bytes: usize,
}

impl Queue {
    fn has_room(&self, bytes: usize) -> bool {
        self.frames.len() < MAX_FRAMES && bytes <= MAX_BYTES - self.bytes
    }
}

impl PlaybackBuffer {
    pub async fn push(&self, frame: Frame) {
        let Some(elapsed) = frame.recording_elapsed.filter(|time| time.is_finite()) else {
            return;
        };
        let bytes = frame.rgb.capacity();
        // Both decoders cap a single RGB frame well below the queue's byte budget.
        if bytes > MAX_BYTES {
            return;
        }
        loop {
            {
                let mut queue = self.queue.lock().unwrap();
                if queue.has_room(bytes) {
                    let index = queue.frames.partition_point(|(time, _)| *time <= elapsed);
                    queue.frames.insert(index, (elapsed, frame));
                    queue.bytes += bytes;
                    return;
                }
            }
            // One producer per queue. notify_one retains a permit if the UI
            // frees space between releasing the lock and awaiting here.
            self.space.notified().await;
        }
    }

    pub fn mark_source_ready(&self) {
        self.source_ready.store(true, Ordering::Relaxed);
    }

    pub fn ready(&self) -> bool {
        self.source_ready.load(Ordering::Relaxed) && !self.queue.lock().unwrap().frames.is_empty()
    }

    /// Keep future frames in order; if rendering fell behind, upload only the newest due frame.
    pub fn take_due(&self, through: f64) -> (Option<Frame>, Option<f64>) {
        let mut queue = self.queue.lock().unwrap();
        let mut latest = None;
        while queue
            .frames
            .front()
            .is_some_and(|(time, _)| *time <= through)
        {
            let (_, frame) = queue.frames.pop_front().unwrap();
            queue.bytes -= frame.rgb.capacity();
            latest = Some(frame);
        }
        let next = queue.frames.front().map(|(time, _)| *time);
        drop(queue);
        if latest.is_some() {
            self.space.notify_one();
        }
        (latest, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(time: f64) -> Frame {
        Frame {
            width: 1,
            height: 1,
            rgb: vec![0; 3],
            recording_elapsed: Some(time),
        }
    }

    #[tokio::test]
    async fn queues_a_burst_in_timestamp_order_and_keeps_future_frames() {
        let buffer = PlaybackBuffer::default();
        for time in [0.0, 0.3, 0.1, 0.2, 0.8, f64::NAN] {
            buffer.push(frame(time)).await;
        }
        assert!(!buffer.ready());
        buffer.mark_source_ready();
        assert!(buffer.ready());
        let (image, next) = buffer.take_due(0.15);
        assert_eq!(image.unwrap().recording_elapsed, Some(0.1));
        assert_eq!(next, Some(0.2));
        let (image, next) = buffer.take_due(0.25);
        assert_eq!(image.unwrap().recording_elapsed, Some(0.2));
        assert_eq!(next, Some(0.3));
        buffer.take_due(f64::INFINITY);
        assert!(!buffer.ready());
    }

    #[tokio::test]
    async fn full_queue_waits_and_delivers_the_next_frame_after_space_is_freed() {
        let buffer = PlaybackBuffer::default();
        for time in 0..MAX_FRAMES {
            buffer.push(frame(time as f64)).await;
        }
        let next = buffer.push(frame(MAX_FRAMES as f64));
        tokio::pin!(next);
        assert!(futures::poll!(&mut next).is_pending());
        assert_eq!(buffer.take_due(0.0).1, Some(1.0));
        assert!(futures::poll!(&mut next).is_ready());
        assert_eq!(
            buffer.take_due(f64::INFINITY).0.unwrap().recording_elapsed,
            Some(MAX_FRAMES as f64)
        );
        assert_eq!(buffer.queue.lock().unwrap().bytes, 0);
    }

    #[test]
    fn byte_budget_applies_even_when_the_frame_count_is_low() {
        let queue = Queue {
            bytes: MAX_BYTES - 5,
            ..Default::default()
        };
        assert!(queue.has_room(5));
        assert!(!queue.has_room(6));
    }
}
