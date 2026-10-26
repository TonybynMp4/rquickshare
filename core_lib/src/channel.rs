use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use ts_rs::TS;

use crate::hdl::info::TransferMetadata;
use crate::hdl::State;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, TS)]
#[ts(export)]
pub enum ChannelDirection {
    #[default]
    FrontToLib,
    LibToFront,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, TS)]
#[ts(export)]
pub enum ChannelAction {
    AcceptTransfer,
    RejectTransfer,
    CancelTransfer,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, TS)]
#[ts(export)]
pub enum TransferType {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct ChannelMessage {
    pub id: String,
    pub direction: ChannelDirection,

    // Only present when channelDirection is frontToLib
    pub action: Option<ChannelAction>,

    // Only present when channelDirection is libToFront
    pub rtype: Option<TransferType>,
    pub state: Option<State>,
    pub meta: Option<TransferMetadata>,
}

/// The latest progress message of every transfer in flight, by transfer id.
///
/// Progress used to go out on the shared ChannelMessage broadcast, held to one
/// message per 100 ms because one per chunk overflowed its 50 slots - and a
/// full bus can drop the user's Cancel, which travels on the same channel. A
/// watch keeps only the newest value, so nothing queues and nothing needs
/// throttling: whoever reads it gets the current progress whenever they look.
static PROGRESS: LazyLock<watch::Sender<HashMap<String, ChannelMessage>>> =
    LazyLock::new(|| watch::channel(HashMap::new()).0);

/// Progress of the transfers in flight. Merge it into the state messages with
/// a [`ProgressGate`].
pub fn subscribe_progress() -> watch::Receiver<HashMap<String, ChannelMessage>> {
    PROGRESS.subscribe()
}

pub(crate) fn publish_progress(msg: ChannelMessage) {
    PROGRESS.send_modify(|board| {
        board.insert(msg.id.clone(), msg);
    });
}

pub(crate) fn clear_progress(id: &str) {
    PROGRESS.send_if_modified(|board| board.remove(id).is_some());
}

fn is_final(state: &State) -> bool {
    matches!(
        state,
        State::Finished | State::Cancelled | State::Rejected | State::Disconnected
    )
}

/// Merges [`subscribe_progress`] with the state messages from the broadcast,
/// for one reader that handles both in a single task.
///
/// The two arrive on different channels, so progress read after a transfer's
/// final state would set its card back to "receiving". The gate drops it, and
/// forwards a transfer's progress only when it has moved.
#[derive(Debug, Default)]
pub struct ProgressGate {
    ended: HashSet<String>,
    shown: HashMap<String, u64>,
}

impl ProgressGate {
    /// Every message from the broadcast, in order, before forwarding it.
    pub fn saw_state(&mut self, msg: &ChannelMessage) {
        if msg.direction != ChannelDirection::LibToFront {
            return;
        }
        match &msg.state {
            Some(state) if is_final(state) => {
                self.ended.insert(msg.id.clone());
                self.shown.remove(&msg.id);
            }
            // Ids can come round again (every BLE receive is "ble"), so any
            // other state is a transfer that is running.
            Some(_) => {
                self.ended.remove(&msg.id);
            }
            None => {}
        }
    }

    /// The progress messages worth forwarding now.
    pub fn fresh(&mut self, board: &HashMap<String, ChannelMessage>) -> Vec<ChannelMessage> {
        let mut out = Vec::new();
        for (id, msg) in board {
            if self.ended.contains(id) {
                continue;
            }
            let acked = msg.meta.as_ref().map_or(0, |m| m.ack_bytes);
            if self.shown.insert(id.clone(), acked) != Some(acked) {
                out.push(msg.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, state: State, acked: u64) -> ChannelMessage {
        ChannelMessage {
            id: id.to_string(),
            direction: ChannelDirection::LibToFront,
            state: Some(state),
            meta: Some(TransferMetadata {
                ack_bytes: acked,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn board(msgs: &[ChannelMessage]) -> HashMap<String, ChannelMessage> {
        msgs.iter().map(|m| (m.id.clone(), m.clone())).collect()
    }

    #[test]
    fn forwards_progress_only_when_it_moved() {
        let mut gate = ProgressGate::default();
        let b = board(&[message("a", State::ReceivingFiles, 10)]);
        assert_eq!(gate.fresh(&b).len(), 1);
        assert!(gate.fresh(&b).is_empty());

        let b = board(&[message("a", State::ReceivingFiles, 20)]);
        assert_eq!(gate.fresh(&b)[0].meta.as_ref().unwrap().ack_bytes, 20);
    }

    #[test]
    fn progress_read_after_the_final_state_is_dropped() {
        let mut gate = ProgressGate::default();
        gate.saw_state(&message("a", State::Finished, 100));
        let stale = board(&[message("a", State::ReceivingFiles, 90)]);
        assert!(gate.fresh(&stale).is_empty());
    }

    #[test]
    fn a_reused_id_gets_progress_again() {
        let mut gate = ProgressGate::default();
        gate.saw_state(&message("ble", State::Finished, 100));
        gate.saw_state(&message("ble", State::WaitingForUserConsent, 0));
        let b = board(&[message("ble", State::ReceivingFiles, 5)]);
        assert_eq!(gate.fresh(&b).len(), 1);
    }

    #[test]
    fn the_board_keeps_only_the_latest_per_transfer() {
        let mut rx = subscribe_progress();
        publish_progress(message("board-test", State::SendingFiles, 1));
        publish_progress(message("board-test", State::SendingFiles, 2));
        let acked = rx.borrow_and_update()["board-test"]
            .meta
            .as_ref()
            .unwrap()
            .ack_bytes;
        assert_eq!(acked, 2);

        clear_progress("board-test");
        assert!(!rx.borrow_and_update().contains_key("board-test"));
    }
}
