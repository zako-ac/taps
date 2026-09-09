# Zako3 Taps
> ⚡️ Official Taps for Zako3

This repository includes official taps for Zako3.

## Included taps
### TTS
* [google](./google-tap/)
* [papago](./papago-tap/)
* [sam](./sam-tap/)
* [supertonic](./supertonic-tap/)
### Music
* [wedding](./wedding-tap/)
* [youtube](./youtube-tap/)

## Protocol

Every tap here is built on [`zakofish4-tap`](https://github.com/zako-ac/zakofish4).
A tap holds one WebSocket to HQ at `/gateway` for control, and sends Opus
straight to whichever sink HQ nominates over protofish4/UDP — the audio never
crosses the control connection.

Point a tap at HQ with `ZAKO_GATEWAY_URL`. There is no certificate to configure
and no SNI: it is an ordinary WebSocket over TLS on the HTTPS port, which is the
whole reason for the change — it survives the NATs and firewalls that the
previous long-lived QUIC connection did not.

`ffmpeg` must be on `PATH`; the taps use the SDK's `auto-encode` helper to turn
downloaded audio into 20 ms Opus frames.
