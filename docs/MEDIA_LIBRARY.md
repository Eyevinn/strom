# Media library

> Code is the source of truth — this may have drifted; read the code for the current implementation.

The **Media** tab manages the files under Strom's media directory (`--media-path`, or
`media/` under the data directory). Media Player and other file-based blocks play from there.
You can browse folders, create and delete them, rename and delete files, download a file to
your computer, and upload files from the browser.

## Upload

Uploading from the browser shows progress per file. One upload request is limited to 500 MB.
An upload replaces a file with the same name without asking.

## Media Player

The Media Player block plays a playlist of library files, and also any URL GStreamer has a
source for: `http(s)` (including HLS and DASH), `rtsp://`, `srt://`, `udp://`, `rtmp://`.
**Video Tracks** and **Audio Tracks** (default 1, 0 discards that kind) set how many tracks of a file get an
output: `video_out`, `video_out_1`, … and `audio_out`, `audio_out_1`, ….

## Download from a URL

Clips that live on a web server, such as stingers, can be copied into the library once instead
of being streamed every time they are cued.

1. Open the folder the file should go into.
2. Paste the URL into **Download from URL** and press **Download** (or Enter).
3. A row shows the progress. When the file is complete it appears in the folder.

Tick **Replace** to overwrite a file with the same name. Without it, Strom refuses before
downloading anything.

The same is available over the API: `POST /api/media/download` with
`{"url": "https://example.com/clips/intro.mov", "path": "stingers"}` (optionally `filename`, and `overwrite` to replace) answers `202` with a job,
`GET /api/media/downloads` lists running and recent jobs, and
`DELETE /api/media/downloads/{job_id}` cancels one. Progress arrives as `MediaDownloadProgress`
events on `/api/ws`. The OpenAPI page (`/swagger-ui`) has the full shapes.

What to expect:

- Only `http` and `https` URLs. The file name comes from the server's `Content-Disposition`
  header, otherwise from the URL. Strom strips path parts and characters a file system cannot
  take.
- A partly downloaded file never shows under its final name. Until it is complete it is a
  hidden `.download-<id>.part` file in the same folder, and it is removed if the download fails
  or is cancelled.
- Files larger than the size limit are refused (2 GiB by default).
- Requests go directly to the server, not through `HTTP_PROXY`/`HTTPS_PROXY`.

### Private and local addresses

Strom fetches on behalf of whoever can call its API, so by default it refuses URLs that lead to
its own host or network: loopback, private ranges (`10/8`, `172.16/12`, `192.168/16`, IPv6
`fc00::/7`), link-local, carrier-grade NAT (`100.64/10`) and similar. It checks the address a
name resolves to, and checks again after every redirect.

To download from a file server on your own network, as in a lab or a single-user setup, allow
private addresses. Cloud metadata addresses (such as `169.254.169.254`) and unroutable
addresses stay refused even then. Leave this off on a Strom that other people or other systems
can reach.

```toml
# .strom.toml
[media]
download_allow_private_addresses = true   # env: STROM_MEDIA_DOWNLOAD_ALLOW_PRIVATE_ADDRESSES=true
download_max_bytes = 2147483648           # env: STROM_MEDIA_DOWNLOAD_MAX_BYTES
download_timeout_seconds = 7200           # env: STROM_MEDIA_DOWNLOAD_TIMEOUT_SECONDS
```
