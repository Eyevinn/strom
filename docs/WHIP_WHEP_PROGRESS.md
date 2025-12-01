# WHIP/WHEP Implementation Progress

## Branch: feature/whip-whep-blocks

### Completed

**WHIP Output Block**
- Audio streaming via `whipclientsink`
- Configurable: endpoint URL, auth token, STUN server
- Audio processing chain: audioconvert -> audioresample -> whipclientsink

**WHEP Input Block**
- WebRTC stream receiving via `whepclientsrc`
- Dynamic pad handling with liveadder mixer for audio
- Silent source keeps pipeline running when no streams connected
- Handles both whepclientsrc pads and internal webrtcbin pads

**WebRTC Statistics**
- Backend API: `GET /api/flows/{id}/webrtc-stats`
- Finds webrtcbin elements (including nested in bins)
- Parses stats using field name prefixes
- Frontend polling (1s interval) for running flows
- UI: compact view on nodes, full view in property inspector

**UI Improvements**
- Human-readable labels for block properties
- Property descriptions shown in UI

### In Progress

**WebRTC Stats Parsing**
- Currently parsing: inbound/outbound RTP, ICE candidates
- Need to verify we're capturing all available stats from webrtcbin
- May be missing: codec stats, transport stats, data channel stats

### Not Started

- TURN server support
- Video support for WHIP output
- WHEP video output handling
