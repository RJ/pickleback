## Changes from upstream Reliable

for sending packets, i need to pass them between threads into webrtc nonsense, so it's no
good havign a transmit_packet fn taking a reference (and let the network stack immediately copy it).
i need to send it via a channel.

so i'm cooking packets into Bytes. Swapped all the packet buffer stuff to use Bytes/BytesMut.

Endpoint now has an outbox, into which it puts "packets" to transmit (Bytes).
All packets received by the Endpoint (ie, via Endpoint::recv), are deemed to be accepted if their
headers parse correctly. Resulting packets returned from ::recv as Bytes with sequence (ReceivedPacket).

no send or recv callback fns needed anymore.

why is reliable using litte endian, will probably change that?

## Fragmenting packets vs msgs

upstream reliable has packets, and fragmented packets. a fragmented packet, comprised of multiple fragment packets, has a single sequence number and is acked only if all fragments are received. so retransmission is a bit inconvenient at the mo. individual fragments are not acked, just the entire thing.

could change endpoint to only send non-fragmented single packets, which are the unit-of-ack. reject anything too big.
then we fragment large messages into smaller ones for sending. acking packets would ack the individual messages containig fragments. unacked reliable messages (which could be fragments of a larger message) will be reincluded in 
outbound packets if they remain unacked for 100ms.

ie, all fragmentation of larger messages is done the layer above endpoint, in servent.

then for sending a burst of initial joining-the-game state, it's a large reliable message, which gets
broken up into multiple reliable messages that are under the mtu. 
messages need to have their own header now:

## Reliable Base Layer

* Doesn't do fragmentation, only accepts payloads under the fragment size / mtu.
* "send payload" --> issues/returns a seqno, builds packet with headers + payload, puts datagram in outbox. (a fixed-size header would allow us to be more efficient with how we allocate payloads - reserving space at the front.)
* "recv datagram" --> parses headers, extracts acks and writes to ack-outbox, puts payload in inbox.

## Message coalescing and reliability layer

* you send "messages", which can be larger than mtu.
* messages get issued a `MessageId`, which is local-only. We map `MessageId` to the packet seqno it was sent in, for acking. Consumers get acks per `MessageId`.
* we coalesce multiple messages into a single packet
* sending a message larger than mtu causes it to be broken up into multiple messages, each with
  their own local msgid, including fragment data in the message header

So on sending fragments, we need to map a list of fragment message ids -> original message id.
and when all fragment messages ids are acked, ack the original.
we can't just ack the original when the packet it was in gets acked. that'll just be the first fragment..

```rust
MessageId {
    num: u32,
    parent_id: Option<u32> // msg is a fragment of a larger msg stored in the fragmap

    fragmented? = parent_id.is_some()
}

// Sender store which msg ids were included in sent packets
// original message ids of messages that got fragmented don't end up in this map
// those get turned into multiple new message ids
AckMap(HashMap<SequenceNumber, Vec<MessageId>>);

// stores unacked messageids. once vec is empty, ack the original message id.
FragMap(HashMap<MessageId, Vec<u32>>)
```
On sender side, when a seqno is acked, we grab the list of now `acked_msg_ids` from the ackmap, 
and remove the entry from the AckMap.

If the MessageId indicates it is unfragmented msg, simply report ack for all `acked_msg_ids` to consumer.

In the case of message being a fragment:
* the fragmented msg id was never issued to a consumer, so don't remote ack to the consumer.
* grab the parent message id and lookup in fragmap. 
* remove ids in acked_msg_ids from the vec of unacked message ids
* if unacked message ids vec is now empty, report the ack of the parent message id to consumer.




### Reliability

Most game network packets are unreliable. Player inputs, state updates, etc.

There are a few small reliable (ordered) messages, like chat.

The important large reliable message is the initial state transfer on joining a game.

If messages are reliable (ie, on a reliable channel?) we store them in an outbox until acked.
we include in packets any unacked messages that were last sent > 100ms ago.

this means if we send a large, reliable, fragmented message, such as intitial game state split into 5 packets,
if one packet doesn't arrive, just the messages left unacked from that packet would be retransmitted.

Also, when we create the fragment messages, we can send them all at once in a burst (up to some limit?).

### Message header

### Small flag
* For non-frag msgs, payload size is a u8 (256b msgs)
* for frags, number of fragments, and also fragment id, uses u8 (256kB payloads)
### Large flag
* For frag msgs, payload size uses 2 bytes, a u16 (can fill packet, up to ~1024B)
* for frags, number of fragments, and also fragment id, uses u16 (65,536 * 1kB = loads)


`MessagePrefixByte`
| bits       |          | description                            |
| ---------- | -------- | -------------------------------------- |
| `-------X` | `<< 0`   | X = 0 non-fragmented. X = 1 fragmented |
| `------X-` | `<< 1`   | X = 0 small flag, X = 1 large flag     |
| `XXXXXX--` | `<< 2-7` | channel number 2^6 = 64 channels       |

## Non-fragmented Message

| bytes  | type       | description                                                                     |
| ------ | ---------- | ------------------------------------------------------------------------------- |
| 1      | `u8`       | `MessagePrefixByte`                                                             |
| 1 or 2 | `u8`/`u16` | Payload Length, 1 or 2 bytes, depending on `MessagePrefixByte` small/large flag |
| ...    | Payload    |                                                                                 |

## Fragmented Message

| bytes  | type          | description                                              |
| ------ | ------------- | -------------------------------------------------------- |
| 1      | `u8`          | `MessagePrefixByte`                                      |
| 1 or 2 | `u8` or `u16` | fragment id, depending on small/large flag               |
| 1 or 2 | `u8` or `u16` | num fragments, depending on small/large flag             |
| 2      | `u16`         | Payload Length, only on last fragment_id. Rest are 1024. |
| ..     | Payload       |                                                          |



## Packet Anatomy

### PrefixByte

Is a `u8` at the start of each packet.

| bits       |              | description                                                                            |
| ---------- | ------------ | -------------------------------------------------------------------------------------- |
| `-------X` | `<< 0`       | X = 0  = regular packet, X = 1 = fragment packet                                       |
| `---XXXX-` | `<< 1,2,3,4` | denotes size of ack mask. each of 4 bits meaning another byte of ack mask data follows |
| `--X-----` | `<<5`        | sequence difference bit                                                                |
| `XX------` | `<<6,7`      | currently unused                                                                       |

### PacketHeader

| bytes              | type             | description                                                                                                     |
| ------------------ | ---------------- | --------------------------------------------------------------------------------------------------------------- |
| 1                  | `u8`             | `PrefixByte`                                                                                                    |
| 2,3                | `u16_le`         | sequence                                                                                                        |
| 4 or 4,5           | `u8` or `u16_le` | sequence_difference, depending on sequnce difference bit in PrefixByte. <br> `sequence` - `last_acked_sequence` |
| 5,6,7,8 or 6,7,8,9 | `u8` x 1-4       | ack bits mask                                                                                                   |

### FragmentHeader

| bytes | type           | description                                         |
| ----- | -------------- | --------------------------------------------------- |
| 1     | `u8`           | `PrefixByte`                                        |
| 2     | `u16_le`       | sequence                                            |
| 3     | `u8`           | fragment id                                         |
| 4     | `u8`           | num fragments                                       |
| 5+    | `PacketHeader` | if frament_id == 0, write the regular packet header |

## Packet Payloads

are basically whatever bytes left after parsing the headers.