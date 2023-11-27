# Packeteer

A way to multiplex and coalesce messages over an unreliable stream of datagrams, for game netcode.

It's expected this will be hooked up to UDP sockets; this crate has no networking built-in.

### Current Status

Under development. I'm RJ on bevy's Discord if you want to discuss.

There is no real protocol, concept of sessions, security features, or anything like that at the moment.
This is just the base reliability layer.

## Design Goals

To support multiplayer games where most updates are sent in an unreliable fashion, with occasional
requirements for reliable ordered messages, such as for in-game chat and on-join state synchronization.

A regular exchange of packets in both directions is expected. In practice this means a packet will
be sent in each direction at least 20 times a second even if there are no explicit messages to transmit.


## Features

- [x] Coalesces multiple small messages into packets
- [x] Transparently fragments & reassembles messages too large for one packet
- [x] Multiple virtual channels for sending/receiving messages
- [x] Optionally reliable channels, with configurable resending behaviour
- [x] Sending a messages gives you a handle, to use for checking packet acks (even on unreliable channels)
- [x] Internal pool of buffers for messages and packets, to minimise allocations
- [x] No async: designed to integrate into your existing game loop. Call it each tick.
- [x] Unit tests and integration / soak tests with bad-link simulator that drops, dupes, & reorders packets.
- [x] Calculates rtt (need to verify outside of test harness)
- [x] Calculate packet loss estimate (need to verify outside of test harness)
- [ ] Enforce ordering on ordered channels (currently only reliability is supported)
- [ ] Bandwidth tracking and budgeting
- [ ] Allow configuration of channels and channel settings (1 reliable, 1 unreliable only atm)
- [ ] Prioritising channels when selecting messages to send. Low volume reliables first?
- [ ] Example using bevy and an unreliable transport mechanism.
- [ ] Benchmarks, including with and without pooled buffers.

## Example

```rust
use packeteer::prelude::*;

// Packeteer is just an endpoint, and server and client are simply names here.
// both ends of the connection behave identically.
let mut server = Packeteer::default();
let mut client = Packeteer::default();

let channel: u8 = 0;

// this can return an error if throttled due to backpressure and unable to send.
// we unwrap here, since it will not fail at this point.
let msg_id: MessageId = server.send_message(channel, b"hello").unwrap();

// update server clock, and transmit server packets to the client
server.update(1.0);
server.drain_packets_to_send().for_each(|packet| {
    // this is where you send the packet over UDP or something
    client.process_incoming_packet(packet.as_ref()).unwrap();
});

// client will have received a message:
let received = client.drain_received_messages(channel).collect::<Vec<_>>();
assert_eq!(received.len(), 1);
// normally you'd use the .payload() to get a Reader, rather than payload_to_owned()
// which reads it into a Vec. But a vec here makes it easier to test.
let recv_payload = received[0].payload_to_owned();
assert_eq!(b"hello".to_vec(), recv_payload);

// if the client doesn't have a message payload to send, it will still send
// an empty packet here just to transmit acks
client.update(1.0);
client.drain_packets_to_send().for_each(|packet| {
    // this is where you send the packet over UDP or something
    server.process_incoming_packet(packet.as_ref()).unwrap();
});

// now the client has sent packets to the server, the server will have received an ack
assert_eq!(vec![msg_id], server.drain_message_acks(channel).collect::<Vec<_>>());
```

## Protocol Overview

Packets consist of a sequence number, an ack header, and a payload.

The ack header acknowledges receipt of the last N packets received from the remote endpoint.

Packet payloads consist of one or more messages. Messages can be any size, and large messages are
fragmented into 1024 byte fragment-messages, and reassembled for you.

When you call `send_message` you get a `MessageId`. Once the packet your message was delivered in is
acked, you receive an ack for your `MessageId`. Unreliable channels also get acks (except if packets are lost).

When sending a message larger than 1024 bytes, you get the ack after all fragments have been delivered,
and the message was reassembled successfully.

Reliable channels will retransmit messages that remain unacked for a configurable duration.

Messages are retransmitted, packets are not. ie as-yet unacked reliable mesages will be included in
new future packets until such time as they get acked.

### Message Size

Arbitrarily limited to 1024 fragments of 1024B, so 1MB maximum messages size.

Remember, as the number of fragments increases, the effects of packet loss are amplified.

10 fragments at 1% packet loss = 1 - (0.99^10) = 9.6% chance of losing a fragment.



### Provenance
* [Gaffer articles](https://gafferongames.com/post/reliable_ordered_messages/) (building network protocol, packet acking, sequence buffer, etc)
* [netcode.io rust code](https://github.com/jaynus/netcode.io/tree/master) (implementation of gaffer concepts)
  
