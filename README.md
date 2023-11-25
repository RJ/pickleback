# Packeteer

A way to multiplex and coalesce messages over an unreliable stream of datagrams, for game netcode.

Typically you hook this up to UDP sockets.

## Features

* Coalesces multiple small messages into packets
* Transparently fragments & reassembles messages too large for one packet
* Multiple virtual channels for sending/receiving messages
* Optionally reliable channels, with configurable resending behaviour
* Sending a messages gives you a handle, to use for checking packet acks
* Internal pool of buffers for messages and packets, to minimise allocations
* No async: designed to integrate into your existing game loop. Call it each tick.
* Unit tests and integration / soak tests with bad-link simulator that drops, dupes, & reorders packets.
* Calculates rtt

## TODO
* Calculate packet loss estimate
* Bandwidth tracking and budgeting
* Allow configuration of channels and channel settings (1 reliable, 1 unreliable only atm)
* Ordering of channels for selecting messages to send
* Example using bevy and an unreliable transport mechanism.
* Perhaps offer a bincoded channel of things that `impl Serialize`?
* Seek feedback on design and public API.
* Benchmark with and without pooled buffers.

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
let received = client.drain_received_messages(0).collect::<Vec<_>>();
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


### Provenance
* [Gaffer articles](https://gafferongames.com/post/reliable_ordered_messages/) (building network protocol, packet acking, sequence buffer, etc)
* [netcode.io rust code](https://github.com/jaynus/netcode.io/tree/master) (implementation of gaffer concepts)
