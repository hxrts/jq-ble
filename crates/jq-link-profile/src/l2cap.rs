//! L2CAP channel helpers for the BLE runtime task.
//!
//! Handles identity exchange on open channels, spawns the read and write
//! tasks that drive a live [`L2capChannel`], and defines
//! [`L2capRuntimeEvent`] for reporting frames and channel closure back to
//! the runtime task's select loop.

use blew::l2cap::L2capChannel;
use blew::types::DeviceId;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::SinkExt;
use jacquard_core::NodeId;
use std::pin::Pin;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::transport::{BleChannelId, DEFAULT_COMMAND_CAPACITY};

// Maximum frame size matches l2cap_endpoint's ByteCount and prevents the codec from accepting oversized frames.
pub(crate) const L2CAP_FRAME_MAX_BYTES: usize = 1472;
// Identity payload is always the full 32-byte NodeId written by the central during channel setup.
const L2CAP_IDENTITY_SIZE_BYTES: usize = 32;

pub(crate) type L2capAcceptStream =
    Pin<Box<dyn Stream<Item = blew::BlewResult<L2capChannel>> + Send + 'static>>;

#[derive(Debug)]
pub(crate) struct ActiveL2capChannel {
    pub(crate) node_id: NodeId,
    pub(crate) device_id: DeviceId,
    pub(crate) outbound_tx: mpsc::Sender<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) enum L2capRuntimeEvent {
    FrameReceived {
        channel_id: BleChannelId,
        payload: Vec<u8>,
    },
    ChannelClosed {
        channel_id: BleChannelId,
    },
}

pub(crate) async fn write_l2cap_identity(
    channel: &mut L2capChannel,
    local_node_id: &NodeId,
) -> Result<(), std::io::Error> {
    // L2CAP accept does not expose the remote DeviceId so the central must introduce itself explicitly.
    tokio::io::AsyncWriteExt::write_all(channel, &local_node_id.0).await?;
    tokio::io::AsyncWriteExt::flush(channel).await?;
    Ok(())
}

pub(crate) async fn read_l2cap_identity(
    channel: &mut L2capChannel,
) -> Result<NodeId, std::io::Error> {
    let mut bytes = [0_u8; L2CAP_IDENTITY_SIZE_BYTES];
    // read_exact blocks until the full 32 bytes arrive to avoid partial-identity races.
    channel.read_exact(&mut bytes).await?;
    Ok(NodeId(bytes))
}

#[must_use]
pub(crate) fn spawn_l2cap_channel_tasks(
    channel_id: BleChannelId,
    channel: L2capChannel,
    event_tx: mpsc::Sender<L2capRuntimeEvent>,
) -> mpsc::Sender<Vec<u8>> {
    // Split the duplex channel into independent reader and writer halves for the two tasks.
    let (reader, writer) = tokio::io::split(channel);
    let mut framed_reader = FramedRead::new(reader, l2cap_codec());
    let mut framed_writer = FramedWrite::new(writer, l2cap_codec());
    let (outbound_tx, mut outbound_rx) =
        mpsc::channel::<Vec<u8>>(DEFAULT_COMMAND_CAPACITY as usize);
    let recv_tx = event_tx.clone();

    // Reader task: decode length-prefixed frames and forward them to the runtime task's select loop.
    tokio::spawn(async move {
        while let Some(frame) = framed_reader.next().await {
            match frame {
                Ok(payload) => {
                    if recv_tx
                        .send(L2capRuntimeEvent::FrameReceived {
                            channel_id,
                            payload: payload.to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Signal closure so the runtime task can clean up the session entry.
        // If the send fails, the runtime task's receiver is gone and cleanup is already moot.
        // allow-ignored-result: closure notification is best-effort when the runtime task already shut down
        let _ = recv_tx
            .send(L2capRuntimeEvent::ChannelClosed { channel_id })
            .await;
    });

    // Writer task: encode and send outbound payloads received from the dispatch path.
    tokio::spawn(async move {
        while let Some(payload) = outbound_rx.recv().await {
            if framed_writer.send(Bytes::from(payload)).await.is_err() {
                break;
            }
        }
    });

    outbound_tx
}

fn l2cap_codec() -> LengthDelimitedCodec {
    // 2-byte LE length header matches the codec convention used in the GATT reliable protocol.
    LengthDelimitedCodec::builder()
        .length_field_length(2)
        .little_endian()
        .max_frame_length(L2CAP_FRAME_MAX_BYTES)
        .new_codec()
}
