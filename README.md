# Packeteer

**A protocol layer for unreliable datagram exchange**

* One instance of `Packeteer` sits at each end of an unreliable datagram channel between two peers.
* `Packeteer` doesn't handle networking, has no concept of sockets or network addresses or client ids.
* A gameserver might have a `HashMap<ClientId, Packeteer>`, and a client would have a single `Packeteer` instance.
* You need to manage sockets/transports/`SocketAddr` yourself and marshal packets with the correct `Packeteer` instance.



#### Example game loop

```rust
fn read_packet_from_network() -> Bytes {
    // however you like
}
fn send_packet_to_network(packet: Bytes) {
    // however you like
}

loop {
    packeteer.update(delta_time)
    // pass received packets to packeteer:
    while let Some(packet) = read_packets_from_network() {
        packeteer.receive_packet(packet);
    }

    simulate_your_game();

    // simulating your game will have enqueued messages to send:
    for packet in packeteer.packets_to_send() {
        send_packet_to_network(packet, ...);
    }
}
```

This library does not do any networking. It is designed to run as part of the game loop – you
call `packeteer.update(delta_time)` at the start of each game tick, along with passing in received
datagrams via `packeteer.receive_packets(...)`. Towards the end of each tick, you must send packets provided by `packeteer.packets_to_send()`.

#### API Overview

You send "messages", which are just `Bytes`. When you call send, packeteer
gives you a message id. Later, you get the ACKs to see if that message was delivered.

```rust
// channels are optionally reliable, optionally ordered.
let channel = MyChannels::Something;

let msg = Bytes::new(...);
let msg_id = packeteer.send_message(channel, msg);

// other end of connection:

for msg in packeteer.drain_received_messages(channel)  {
    // ...
}

// later / next tick

let new_message_id_acks = packeteer.drain_message_acks();
if new_message_id_acks.contains(msg_id) {
    println!("{msg_id} was acked");
}

```

Under the hood, small messages are coalesced into packets to be sent. Larger messages are fragmented
and send using multiple packets for you.



### Channel Types

Messages are sent on channels, which are configured to optionally reliable and or ordered.





### Packet Layer

fork of reliable, can only send packets under the mtu (no fragmenting).
handles packet level acks.

### Message Layer

small messages are coalesced into packets, and acked per message to consumer.
large messages are fragmented into multiple smaller messages and reassembled.




## reliable messages

send/receive channel. like the inbox/outbox, but hides the reliablility or ordering bits?
inbox: packets go in, messages go to outbbox
outbox: during update() can write resends to outbox too


# What happens per layers

## Base Packet Layer

base layer is the sending of unreliable, unordered, unfragmented, less-than-mtu size datagrams, with acks.
the packet header ackfield handles acking of packets.
all this layer does is add a thin header to packets to transmit acks and packet seqnos.

## ??
```rust
api.send_message(channel, payload)

let channel = api.get_channel(channel);
// this will either add a single msg, or lots of fragments, to the channel:
let msg_id = dispatcher.add_message_to_channel(&mut channel, payload);
// and later.
let messages_received: Vec<Message> = dispatcher.process_message(&mut channel, message);
// (empty if a fragment of incomplete msg)

```


# Notes

each message needs an id on the wire, since that's the only way we can dedupe on receiving end?
then we prevent duplicated  msgs being received in any channel (and can reorder, drop stale.)

it's a problem when we resend a msg because ack is late arriving - it arrives twice.


