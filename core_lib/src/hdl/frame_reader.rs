//! Reads length-prefixed frames off a transport in a task of its own.
//!
//! **Why this exists.** Both request handlers used to do:
//!
//! ```ignore
//! tokio::select! {
//!     i = self.receiver.recv() => { ... }
//!     h = stream_read_exact(&mut self.socket, &mut length_buf) => { ... }
//! }
//! ```
//!
//! `stream_read_exact` is not cancel-safe. When the channel branch wins while
//! part of a length prefix has already been read, that read future is dropped
//! and the consumed bytes are gone - the stream desyncs and the next frame
//! parses as garbage. `receiver` is `sender.subscribe()`, so the handler's *own*
//! progress broadcasts come back to it and fire that branch repeatedly during a
//! transfer, which is the intermittent "Missing required fields" seen mid-
//! handshake with reassembly provably clean.
//!
//! Reading in a dedicated task removes the cancellation entirely: nothing races
//! a partial read. The handler then selects over two channels, both of which
//! *are* cancel-safe.
//!
//! It also decouples reading from writing, which is what lets a long send answer
//! keepalives. The send loop writes chunk after chunk without reading, so over
//! BLE - where a 1.9 MB file takes ~95s at 20 KB/s - the peer saw no KeepAlive
//! response and closed the connection at its 30s timeout, capping outbound
//! transfers at whatever fits in 30s.
//!
//! This task does **framing only**. It never decrypts, so there is no shared
//! crypto state and no mutex: sequence numbers and keys stay entirely inside the
//! handler.

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use crate::utils::stream_read_exact;

/// A frame as it came off the wire, still encrypted.
pub type RawFrame = Vec<u8>;

/// Spawn a reader over `reader`, returning the channel its frames arrive on.
///
/// The channel is bounded so a peer cannot make us buffer without limit: if the
/// handler is busy, the reader stops reading and the transport applies its own
/// backpressure. Four frames is enough to keep the handler fed without letting
/// a fast sender run far ahead.
///
/// The task ends as soon as the handler drops the receiver, even mid-read. A
/// split stream only closes once both halves are gone, so a reader that waited
/// for the peer would hold the connection open for as long as a silent peer
/// liked. Cancelling that read is safe here: nobody is left to want the frame.
pub fn spawn_frame_reader<R>(mut reader: R, sane_frame_length: usize) -> mpsc::Receiver<RawFrame>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<RawFrame>(4);

    tokio::spawn(async move {
        loop {
            let frame = tokio::select! {
                _ = tx.closed() => {
                    trace!("frame reader: handler finished, releasing the transport");
                    break;
                }
                frame = read_frame(&mut reader, sane_frame_length) => frame,
            };
            let Some(frame) = frame else {
                break;
            };

            if tx.send(frame).await.is_err() {
                // Handler is gone.
                break;
            }
        }
    });

    rx
}

/// One length-prefixed frame, or `None` once the transport is finished: EOF,
/// a read error, or a length we refuse.
async fn read_frame<R>(reader: &mut R, sane_frame_length: usize) -> Option<RawFrame>
where
    R: AsyncRead + Unpin,
{
    let mut length_buf = [0u8; 4];
    if let Err(e) = stream_read_exact(reader, &mut length_buf).await {
        // EOF is the normal end of a session; anything else is worth a line,
        // but neither is fatal here - dropping `tx` tells the handler the
        // transport is finished.
        trace!("frame reader: read ended: {e}");
        return None;
    }

    let msg_length = u32::from_be_bytes(length_buf) as usize;
    if msg_length == 0 || msg_length > sane_frame_length {
        error!("frame reader: refusing insane frame length {msg_length}");
        return None;
    }

    let mut frame = vec![0u8; msg_length];
    if let Err(e) = reader.read_exact(&mut frame).await {
        trace!("frame reader: incomplete frame: {e}");
        return None;
    }
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer that connects and then says nothing must not keep the reader,
    /// and so the connection, alive once the handler has finished with it.
    #[tokio::test]
    async fn reader_releases_the_transport_when_the_handler_is_done() {
        let (ours, mut theirs) = tokio::io::duplex(64);
        let (read_half, write_half) = tokio::io::split(ours);
        let frames = spawn_frame_reader(read_half, 1024);

        // The handler finishing drops its write half and the frame channel.
        drop(write_half);
        drop(frames);

        // Our end closes only once the reader has let go of its half too; the
        // peer sees that as EOF. The timeout only turns a hang into a failure.
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(std::time::Duration::from_secs(10), theirs.read(&mut buf))
            .await
            .expect("the reader kept the transport open")
            .unwrap();
        assert_eq!(n, 0);
    }
}
