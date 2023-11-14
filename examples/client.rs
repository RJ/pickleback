extern crate packeteer;
use bytes::{BufMut, Bytes, BytesMut};
use packeteer::*;
const NUM_TEST_MSGS: usize = 10;
const NUM_EXTRA_ITERATIONS: usize = 100;
use log::*;
fn random_payload(size: u32) -> Bytes {
    let mut b = BytesMut::with_capacity(size as usize);
    for _ in 0..size {
        b.put_u8(rand::random::<u8>());
    }
    b.freeze()
}

fn main() {
    let _ = env_logger::builder()
        .write_style(env_logger::WriteStyle::Always)
        .try_init();
    // crate::test_utils::init_logger();
    let mut server = Packeteer::new(1_f64);
    let mut client = Packeteer::new(1_f64);

    let channel = 1;

    let mut server_jitter_pipe = JitterPipe::<Bytes>::new(JitterPipeConfig::default());
    let mut client_jitter_pipe = JitterPipe::<Bytes>::new(JitterPipeConfig::default());

    let mut test_msgs = Vec::new();
    (0..NUM_TEST_MSGS)
        .for_each(|_| test_msgs.push(random_payload(rand::random::<u32>() % (1024 * 16))));

    let mut client_received_messages = Vec::new();
    let mut client_received_acks = Vec::new();

    for i in 0..(NUM_TEST_MSGS + NUM_EXTRA_ITERATIONS) {
        server.update(i as f64 * 0.051);
        client.update(i as f64 * 0.051);

        if let Some(msg) = test_msgs.get(i) {
            let size = msg.len();
            let msg_id = server.send_message(channel, msg.clone());
            println!("💌 Sending message of size {size}, msg_id: {msg_id}");
        }

        server.drain_packets_to_send().for_each(|packet| {
            info!("-->pipe");
            server_jitter_pipe.insert(packet);
        });

        while let Some(p) = server_jitter_pipe.take_next() {
            client.process_incoming_packet(p);
        }

        client
            .drain_packets_to_send()
            .for_each(|packet| client_jitter_pipe.insert(packet));
        while let Some(p) = client_jitter_pipe.take_next() {
            server.process_incoming_packet(p);
        }

        client_received_messages.extend(client.drain_received_messages(channel));
        client_received_acks.extend(server.drain_message_acks(channel));
    }
}

// fn print_packets(mut ev: ResMut<Events<ReceivedPacket>>) {
//     for ReceivedPacket {
//         slot,
//         received_at,
//         payload,
//     } in ev.drain()
//     {
//         info!("PRINT_PACKETS {slot} = {payload:?}");
//     }
// }
