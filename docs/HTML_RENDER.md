# HTML Rendering with CEF (Chromium Embedded Framework)

Strom supports rendering HTML content as video sources using the `cefsrc` GStreamer element from [gstcefsrc](https://github.com/AioCef/gstcefsrc). This enables:

- Dynamic HTML/CSS/JavaScript overlays
- Web-based graphics and animations
- Real-time data visualization
- Chromium-powered web content as video input

## Docker Image

HTML rendering requires Chromium Embedded Framework (CEF), which adds ~1.5GB to the image size. To keep the base image lightweight, this functionality is available in a separate extended image:

| Image | Size | Use Case |
|-------|------|----------|
| `eyevinntechnology/strom:latest` | ~200MB | Standard pipelines (no HTML rendering) |
| `eyevinntechnology/strom-full:latest` | ~1.7GB | Full functionality including HTML rendering |

### Quick Start

```bash
# Pull the full image
docker pull eyevinntechnology/strom-full:latest

# Run with host networking (recommended for multicast/AES67)
docker run --network host eyevinntechnology/strom-full:latest

# Or with port mapping
docker run -p 8080:8080 eyevinntechnology/strom-full:latest
```

## Using cefsrc in Pipelines

The `cefsrc` element renders a URL to video frames. Basic properties:

| Property | Type | Description |
|----------|------|-------------|
| `url` | string | URL to render (http://, https://, file://, or data:) |

### Example: Import via gst-launch

In the Strom UI, use "Import gst-launch" to add a cefsrc pipeline:

```bash
cefsrc url=https://example.com ! videoconvert ! autovideosink
```

### Example: API

```bash
# Parse pipeline to flow elements
curl -X POST http://localhost:8080/api/gst-launch/parse \
  -H "Content-Type: application/json" \
  -d '{"pipeline": "cefsrc url=https://example.com ! videoconvert ! fakesink"}'
```

### Example: Transparent Overlay (Data URL)

This example renders a bouncing ball on a transparent background, useful for overlays:

```json
{
  "id": "00000000-0000-0000-0000-000000000002",
  "name": "ball overlay",
  "elements": [
    {
      "id": "cefsrc_0",
      "element_type": "cefsrc",
      "properties": {
        "url": "data:text/html,<style>body{margin:0;background:transparent}</style><canvas id=c></canvas><script>const c=document.getElementById('c'),x=c.getContext('2d');c.width=1920;c.height=1080;let bx=100,by=100,dx=4,dy=3;function d(){x.clearRect(0,0,1920,1080);x.beginPath();x.arc(bx,by,60,0,Math.PI*2);x.fillStyle='%23ff6b6b';x.fill();x.strokeStyle='%23fff';x.lineWidth=4;x.stroke();bx+=dx;by+=dy;if(bx>1860||bx<60)dx=-dx;if(by>1020||by<60)dy=-dy;requestAnimationFrame(d)}d()</script>"
      },
      "position": [100.0, 200.0]
    }
  ],
  "blocks": [],
  "links": []
}
```

### Example: Import Flow JSON

Import this flow via the UI (Import → JSON) to render a live wind map with WHEP output:

```json
{
  "id": "00000000-0000-0000-0000-000000000001",
  "name": "html render",
  "elements": [
    {
      "id": "cefsrc_0",
      "element_type": "cefsrc",
      "properties": {
        "url": "https://earth.nullschool.net/#current/wind/surface/level/orthographic=13.01,61.06,1232"
      },
      "position": [100.0, 200.0]
    }
  ],
  "blocks": [
    {
      "id": "whep_0",
      "block_definition_id": "builtin.whep_output",
      "properties": {
        "mode": "video",
        "endpoint_id": "html render"
      },
      "position": {"x": 400.0, "y": 200.0}
    }
  ],
  "links": [
    {
      "from": "cefsrc_0:src",
      "to": "whep_0:video_in"
    }
  ]
}
```

## How It Works

The `strom-full` Docker image includes:

1. **gstcefsrc plugin** - GStreamer plugin providing `cefsrc`, `cefdemux`, and `cefbin` elements
2. **Xvfb** - X Virtual Framebuffer for headless rendering
3. **CEF runtime** - Chromium libraries, locales, and resources

### Automatic Configuration

The entrypoint script automatically:

- Starts Xvfb on display `:99`
- Disables CEF sandbox (required for Docker root user)
- Disables GPU (Xvfb is software-only)
- Configures CEF cache and logging

No manual configuration is needed - just run the container and use `cefsrc` in your pipelines.

## Troubleshooting

### "Missing X server or $DISPLAY"

The Xvfb server may not have started. Check container logs:

```bash
docker logs <container_id>
```

Verify Xvfb is running:

```bash
docker exec <container_id> ps aux | grep Xvfb
```

### "locale_file_path.empty() for locale"

CEF can't find its locale files. This is fixed in strom-full:0.3.12+. Ensure you're using the latest image:

```bash
docker pull eyevinntechnology/strom-full:latest
```

### DBus errors in logs

Messages like "Failed to connect to the bus" are benign warnings - DBus is not available in the container but CEF works without it.

### High CPU usage

CEF renders pages continuously. For static content, consider:
- Using simpler HTML/CSS
- Reducing resolution if full HD isn't needed
- Setting appropriate framerate in your pipeline

## Building gstcefsrc

The gstcefsrc plugin is pre-built and included in the strom-full image. For manual builds:

```bash
# Build the gstcefsrc plugin
cd docker/gstcefsrc
docker build --platform linux/amd64 -t gstcefsrc-builder:amd64 .

# Extract built files
docker run --rm -v $(pwd)/output:/export gstcefsrc-builder:amd64
```

The build uses Ubuntu Questing to match the strom base image's glibc version.

## Limitations

- **Docker only**: CEF requires X11, which the strom-full image provides via Xvfb
- **Software rendering**: GPU acceleration is disabled in Docker; rendering is CPU-based
- **Memory usage**: CEF spawns multiple processes (browser, renderer, GPU process)
- **No audio by default**: Use `cefbin` or `cefdemux` if you need audio from web content

## References

- [gstcefsrc GitHub](https://github.com/AioCef/gstcefsrc) - GStreamer CEF plugin
- [CEF Project](https://bitbucket.org/AioCef/cef/overview) - Chromium Embedded Framework
