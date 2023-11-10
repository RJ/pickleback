# Packeteer

**A transport layer atop unreliable datagram exchange**

This library expects you to marshall packets via an unreliable datagram exchange mechanism.
In my case, webrtc unreliable datachannels.


### Packet Layer

fork of reliable, can only send packets under the mtu (no fragmenting).
handles packet level acks.

### Message Layer

small messages are coalesced into packets, and acked per message to consumer.
large messages are fragmented into multiple smaller messages and reassembled.




