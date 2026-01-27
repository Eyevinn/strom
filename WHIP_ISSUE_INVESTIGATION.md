# WHIP Issue Investigation - 2026-01-27

## Problem
WHIP-anslutningar från strom till intercom-manager/SMB kraschar när en browser ansluter till samma intercom-rum.

## Setup

```
┌─────────────────────────────────────────────────────────────────────────┐
│  sto-th2-neo-br-pr01 (STROM)                                            │
│  ┌─────────┐      ┌──────────┐      ┌──────────┐      ┌─────────┐      │
│  │ AES67   │──────│ 8ch      │──────│ WHIP x4  │──────│ 4 rum   │      │
│  │ Input   │      │ split    │      │ outputs  │      │ (1-way) │      │
│  └─────────┘      └──────────┘      └──────────┘      └────┬────┘      │
│                                                            │           │
│  ┌─────────┐      ┌──────────┐      ┌──────────┐           │           │
│  │ AES67   │◀─────│ 8ch      │◀─────│ WHEP x4  │◀──────────┘           │
│  │ Output  │      │ merge    │      │ inputs   │     (1-way retur)     │
│  └─────────┘      └──────────┘      └──────────┘                       │
└─────────────────────────────────────────────────────────────────────────┘
                           │                    ▲
                    WHIP   │                    │  WHEP
                    (send) ▼                    │  (recv)
┌─────────────────────────────────────────────────────────────────────────┐
│  sto-neo-com-smb-dev01 (SMB/Intercom)                                   │
│  ┌──────────────────────────────────────────────────────────────┐      │
│  │  neocom-intercom-manager                                      │      │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐          │      │
│  │  │  Rum 1  │  │  Rum 2  │  │  Rum 3  │  │  Rum 4  │          │      │
│  │  └─────────┘  └─────────┘  └─────────┘  └─────────┘          │      │
│  └──────────────────────────────────────────────────────────────┘      │
└─────────────────────────────────────────────────────────────────────────┘
```

- Ljud kommer till strom från AES67
- Split till 4 olika rum i intercom via WHIP (enkelriktat, send-only)
- WHEP tar emot ljud tillbaka från rummen (enkelriktat, recv-only)
- Merge till 8 kanaler ut på AES67

## Symptom

1. Allt fungerar vid uppstart - ljud flödar hela vägen runt
2. En browser ansluter till ett rum och börjar prata
3. GStreamer loggar `internal datastream error` för `queue2`/`nicesrc0`
4. Efter en stund: WHIP-kopplet till det rummet går ner
5. SMB säger "Removing idle stream" efter ~60 sek
6. Ljudet från strom till det rummet slutar

## Rotorsak (bekräftad)

### GStreamer-sidan (strom)
```
0:00:34.839 - webrtcbin3: session 0 ssrc 4285571184 new ssrc  <-- Browser ansluter
0:00:34.839 - ERROR: nicesrc0 "streaming stopped, reason not-linked"
0:01:02   - whip-webrtcbin: ICE connection: completed → FAILED
```

Felet är i `TransportReceiveBin:transportreceivebin2/GstNiceSrc:nicesrc0` - detta är receive-delen av WebRTC-transporten i whipsink. Den tar emot data men har inget att göra med den ("not-linked").

### SMB-sidan (intercom)
```
08:18:30.972 [ActiveMediaList-1548] new audio endpoint 44fe5399-893f-49c7-a7d3-b7b57286195a
                                    mapped ssrc -> 4285571184
```

SMB mappar browserns audio till SSRC 4285571184 och skickar detta till ALLA endpoints i rummet - inklusive WHIP-endpointen från strom.

### Problemflöde
```
1. Whipsink skickar SDP offer med "sendonly"
2. Intercom-manager svarar med "recvonly" i SDP (rad 432-433 i api_productions_core_functions.ts)
3. Men SMB konfigureras UTAN direction-info (smb.ts har inget stöd för direction)
4. SMB behandlar WHIP-endpoint som vanlig deltagare och skickar media dit
5. Whipsink kan inte hantera inkommande RTP → "not-linked" error
6. GStreamer pipeline kraschar
7. RTP-paket slutar skickas
8. SMB timeout:ar efter 60 sek och tar bort streamen
```

## Tekniska detaljer

### Relevanta filer i intercom-manager
- `/home/per/src/svt/intercom-manager/src/smb.ts` - SMB-protokollhantering (ingen direction-support)
- `/home/per/src/svt/intercom-manager/src/api_productions_core_functions.ts` - Core endpoint-konfiguration
  - Rad 432-433: Direction-inversion i SDP svar
- `/home/per/src/svt/intercom-manager/src/api_whip.ts` - WHIP endpoint
- `/home/per/src/svt/intercom-manager/src/models.ts` - SmbEndpointDescription (saknar neighbours-fält)

### SMB API-begränsning
SMB:s API har **inget stöd för direction** (recvonly/sendonly). Det finns bara `relay-type` som styr hur media vidarebefordras, men inte OM media ska skickas till en endpoint.

Dokumentation: https://github.com/finos/SymphonyMediaBridge/blob/master/doc/api/READMEapi.md

### SMB neighbours.groups - undersökt men fungerar INTE för vårt problem

SMB har `neighbours.groups` parameter vid endpoint-konfiguration:
```json
"neighbours": {
    "groups": ["group1", "12345"]
}
```

**Enligt SMB wiki:**
> "Neighbour support where participants in the same acoustic group will **not hear each other**. Avoids acoustic double talk."

**Hur det fungerar:**
- Endpoints i **samma** `neighbours.groups` → hör **INTE** varandra
- Endpoints i **olika** grupper → hör varandra

**Varför det inte löser vårt problem:**
- neighbours.groups förhindrar bara att endpoints i SAMMA grupp hör varandra
- WHIP-endpoint skulle fortfarande få media från endpoints i ANDRA grupper
- Vi behöver att WHIP-endpoint inte får NÅGON media alls

### SMB outbound context

SMB skapar automatiskt `audio outbound context` för VARJE endpoint som har en audioStream:
```
[EngineMixer-1642] Created new audio outbound context for stream, endpointIdHash X, ssrc Y
```

Det finns inget sätt att skapa en "ingest-only" endpoint i SMB som bara tar emot utan att skicka tillbaka.

### Alternativ som undersökts och INTE fungerar

| Alternativ | Varför det inte fungerar |
|------------|--------------------------|
| `neighbours.groups` | Förhindrar bara endpoints i SAMMA grupp att höra varandra |
| `relay-type` | Styr bara hur ljud mixas (forwarder/mixed/ssrc-rewrite), inte OM det skickas |
| Tom audio-konfiguration | Då kan SMB inte ta emot ljud från WHIP heller |
| SDP direction (recvonly/sendonly) | Påverkar bara SDP-signalering, SMB bryr sig inte |

## Lösningsalternativ (uppdaterad)

| # | Lösning | Plats | Komplexitet | Beskrivning |
|---|---------|-------|-------------|-------------|
| **1** | Fixa whipsink | gst-plugins-rs | Medel | Modifiera att ignorera/droppa inkommande RTP istället för att krascha |
| **2** | Bidra till SMB | SMB upstream | Hög | Lägga till direction-stöd i SMB API |
| **3** | Workaround | intercom-manager | Låg | Sätt WHIP + browser i samma neighbours.group (delvis lösning) |

### Alternativ 1 - Rekommenderas

Fixa i GStreamer whipsink (`gst-plugins-rs`) så att den hanterar inkommande RTP gracefully:
- Antingen ignorera/droppa inkommande RTP-paket
- Eller koppla nicesrc till en fakesink/null

Detta är den renaste lösningen eftersom WHIP per definition är send-only.

### Alternativ 2 - Upstream fix

Bidra direction-stöd till Symphony Media Bridge:
- Lägg till `direction: "sendonly" | "recvonly" | "sendrecv"` i endpoint-konfiguration
- SMB skapar inte outbound context för `sendonly` endpoints

Kräver upstream-acceptans och är mer långsiktigt.

### Alternativ 3 - Workaround (delvis)

I intercom-manager, sätt WHIP-endpoint och browser i samma `neighbours.groups`:
- Kräver att lägga till `neighbours` i `SmbEndpointDescription` (models.ts)
- Sätt samma grupp-ID för WHIP och alla browsers i samma rum

**Begränsning:** Fungerar bara om det finns EN browser i rummet. Om det finns flera browsers i olika grupper, får WHIP fortfarande media från dem.

## Servrar

| Server | Roll | Container |
|--------|------|-----------|
| sto-th2-neo-br-pr01 | STROM (GStreamer pipelines) | strom2 |
| sto-neo-com-smb-dev01 | SMB (Symphony Media Bridge) | smb |

## Kommandon för felsökning

```bash
# Strom-loggar
ssh sto-th2-neo-br-pr01 "docker logs strom2 --tail 500 2>&1"

# SMB-loggar
ssh peen04@sto-neo-com-smb-dev01 "docker exec smb cat /tmp/smb.log" | tail -200

# Mer debug i GStreamer
GST_DEBUG=nice:4,dtls:4,webrtcbin:4

# Kolla SMB outbound contexts
ssh peen04@sto-neo-com-smb-dev01 "docker exec smb grep -E 'outbound.*context|Created.*audio' /tmp/smb.log"
```

## Nästa steg

1. **Kortsiktigt:** Undersök om whipsink i gst-plugins-rs kan modifieras att hantera inkommande RTP
2. **Medellångsiktigt:** Överväg att bidra direction-stöd till SMB upstream
3. **Alternativt:** Implementera neighbours.groups workaround i intercom-manager (begränsad lösning)

## Referenser

- SMB GitHub: https://github.com/finos/SymphonyMediaBridge
- SMB API: https://github.com/finos/SymphonyMediaBridge/blob/master/doc/api/READMEapi.md
- SMB Wiki: https://github.com/finos/SymphonyMediaBridge/wiki
- gst-plugins-rs whipsink: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs
