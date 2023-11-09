# Bevy Reparate

This library expects you to marshall packets via an unreliable datagram exchange mechanism.
In my case, webrtc unreliable datachannels.

## Layers

seems like it could be a nice separation of concerns to have a bevy app that coalesces messages into packets,
handles retransmissions, manages acking of messages, offers un/reliable messages.


