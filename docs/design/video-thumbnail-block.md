# Video Thumbnail Block Design

## Overview

Add a reusable video thumbnail/preview block that can be inserted anywhere in a flow to capture and stream video frames to the frontend. This follows the existing `builtin.meter` pattern for audio level monitoring.

## Motivation

- Enable video preview in the compositor layout editor
- Provide visual feedback for any video stream (transcoding, mixing, etc.)
- Reusable component - not tied to any specific block type
- Follows established patterns in the codebase

## Current Audio Meter Pattern (Reference)

The existing meter block provides a proven architecture to follow:

```
Audio → [Meter Block] → Audio (pass-through)
              ↓
        GStreamer "level" element posts bus messages
              ↓
        Backend broadcasts StromEvent::MeterData via WebSocket
              ↓
        Frontend MeterDataStore with 500ms TTL
              ↓
        Visualized in graph nodes + property inspector
```

## Proposed Thumbnail Architecture

```
Video → [Thumbnail Block] → Video (pass-through)
              ↓
        tee → videoscale → jpegenc → appsink
              ↓
        Backend broadcasts StromEvent::ThumbnailData via WebSocket
              ↓
        Frontend ThumbnailStore with TTL
              ↓
        Visualized in graph nodes + property inspector + compositor editor
```

## Block Design: `builtin.thumbnail`

### Properties

| Property | Type | Default | Range | Description |
|----------|------|---------|-------|-------------|
| `width` | u32 | 160 | 80-640 | Thumbnail width in pixels |
| `height` | u32 | 90 | 45-360 | Thumbnail height in pixels |
| `interval_ms` | u32 | 500 | 100-2000 | Update interval in milliseconds |
| `quality` | u32 | 75 | 50-95 | JPEG compression quality |

### Internal Pipeline

```
video_in → tee ─┬─ queue → video_out (passthrough, caps preserved)
                │
                └─ queue → videoscale → videoconvert → jpegenc → appsink
                           (thumbnail branch, scaled down)
```

### Metadata

- **Block ID**: `builtin.thumbnail`
- **Category**: Analysis
- **Color**: `#2196F3` (blue, similar to video-related blocks)
- **Icon**: 🖼️ or 📷
- **Size**: 1.5 × 2.0 (same as meter)

## Implementation Plan

### Backend Components

| File | Description |
|------|-------------|
| `backend/src/blocks/builtin/thumbnail.rs` | New block with tee + appsink pipeline |
| `backend/src/blocks/builtin/mod.rs` | Register new block |
| `types/src/events.rs` | Add `StromEvent::ThumbnailData` variant |

### Frontend Components

| File | Description |
|------|-------------|
| `frontend/src/thumbnail.rs` | ThumbnailStore - stores decoded textures |
| `frontend/src/app.rs` | Handle ThumbnailData events, render in graph |
| `frontend/src/properties.rs` | Full preview in property inspector |
| `frontend/src/compositor_editor.rs` | Use thumbnails for compositor inputs |

## Detailed Implementation

### 1. Event Type (`types/src/events.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StromEvent {
    // ... existing variants ...

    /// Video thumbnail data from a thumbnail block
    ThumbnailData {
        flow_id: String,
        element_id: String,
        width: u32,
        height: u32,
        jpeg_base64: String,  // Base64-encoded JPEG data
        timestamp_ns: u64,    // Frame PTS for ordering
    },
}
```

### 2. Thumbnail Block (`backend/src/blocks/builtin/thumbnail.rs`)

```rust
use gst::prelude::*;

pub struct ThumbnailBuilder;

impl BlockBuilder for ThumbnailBuilder {
    fn build(&self, ctx: BlockBuildContext) -> Result<BlockBuildResult> {
        let instance_id = &ctx.instance_id;

        // Get properties with defaults
        let width = ctx.properties.get("width")
            .and_then(|v| v.as_u64())
            .unwrap_or(160) as u32;
        let height = ctx.properties.get("height")
            .and_then(|v| v.as_u64())
            .unwrap_or(90) as u32;
        let interval_ms = ctx.properties.get("interval_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(500) as u64;
        let quality = ctx.properties.get("quality")
            .and_then(|v| v.as_u64())
            .unwrap_or(75) as i32;

        // Create elements
        let tee = gst::ElementFactory::make("tee")
            .name(format!("{}:tee", instance_id))
            .build()?;

        let passthrough_queue = gst::ElementFactory::make("queue")
            .name(format!("{}:passthrough_queue", instance_id))
            .property("max-size-buffers", 3u32)
            .build()?;

        let thumbnail_queue = gst::ElementFactory::make("queue")
            .name(format!("{}:thumbnail_queue", instance_id))
            .property("max-size-buffers", 1u32)
            .property("leaky", 2u32) // downstream
            .build()?;

        let videoscale = gst::ElementFactory::make("videoscale")
            .name(format!("{}:videoscale", instance_id))
            .build()?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(format!("{}:capsfilter", instance_id))
            .property("caps", gst::Caps::builder("video/x-raw")
                .field("width", width as i32)
                .field("height", height as i32)
                .build())
            .build()?;

        let videoconvert = gst::ElementFactory::make("videoconvert")
            .name(format!("{}:videoconvert", instance_id))
            .build()?;

        let jpegenc = gst::ElementFactory::make("jpegenc")
            .name(format!("{}:jpegenc", instance_id))
            .property("quality", quality)
            .build()?;

        let appsink = gst::ElementFactory::make("appsink")
            .name(format!("{}:appsink", instance_id))
            .property("emit-signals", true)
            .property("max-buffers", 1u32)
            .property("drop", true)
            .build()?;

        // Set up rate limiting via frame interval
        // (Could use videorate element or manual sample dropping)

        let elements = vec![
            tee.clone(),
            passthrough_queue.clone(),
            thumbnail_queue,
            videoscale,
            capsfilter,
            videoconvert,
            jpegenc,
            appsink.clone(),
        ];

        // Links would be set up in build process...
        // tee.src_0 → passthrough_queue → (output pad)
        // tee.src_1 → thumbnail_queue → videoscale → capsfilter → videoconvert → jpegenc → appsink

        // Set up appsink callback for thumbnail extraction
        let flow_id = ctx.flow_id.clone();
        let element_id = instance_id.clone();
        let broadcaster = ctx.event_broadcaster.clone();

        // This would be set up as a bus message handler or appsink callback
        // Similar to how meter.rs handles level messages

        Ok(BlockBuildResult {
            elements,
            internal_links: vec![/* ... */],
            bus_message_handler: Some(/* thumbnail extraction handler */),
            // ...
        })
    }
}
```

### 3. Frontend Store (`frontend/src/thumbnail.rs`)

```rust
use egui::TextureHandle;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Key for thumbnail lookup
pub type ThumbnailKey = (String, String); // (flow_id, element_id)

/// Cached thumbnail data
pub struct ThumbnailData {
    pub texture: TextureHandle,
    pub width: u32,
    pub height: u32,
    pub last_update: Instant,
}

/// Store for video thumbnails received via WebSocket
pub struct ThumbnailStore {
    thumbnails: HashMap<ThumbnailKey, ThumbnailData>,
    max_age: Duration,
}

impl ThumbnailStore {
    pub fn new() -> Self {
        Self {
            thumbnails: HashMap::new(),
            max_age: Duration::from_secs(2), // Longer TTL than audio (less frequent updates)
        }
    }

    /// Update thumbnail from WebSocket event
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        flow_id: &str,
        element_id: &str,
        width: u32,
        height: u32,
        jpeg_data: &[u8],
    ) {
        // Decode JPEG to RGBA
        let image = image::load_from_memory(jpeg_data)
            .ok()
            .map(|img| img.to_rgba8());

        if let Some(rgba) = image {
            let texture = ctx.load_texture(
                format!("thumb_{}_{}", flow_id, element_id),
                egui::ColorImage::from_rgba_unmultiplied(
                    [width as usize, height as usize],
                    rgba.as_raw(),
                ),
                egui::TextureOptions::LINEAR,
            );

            self.thumbnails.insert(
                (flow_id.to_string(), element_id.to_string()),
                ThumbnailData {
                    texture,
                    width,
                    height,
                    last_update: Instant::now(),
                },
            );
        }
    }

    /// Get thumbnail for display
    pub fn get(&self, flow_id: &str, element_id: &str) -> Option<&ThumbnailData> {
        let key = (flow_id.to_string(), element_id.to_string());
        self.thumbnails.get(&key).filter(|t| t.last_update.elapsed() < self.max_age)
    }

    /// Remove stale thumbnails
    pub fn cleanup_stale(&mut self) {
        self.thumbnails.retain(|_, v| v.last_update.elapsed() < self.max_age);
    }

    /// Remove thumbnails for a specific flow (when flow is deleted)
    pub fn remove_flow(&mut self, flow_id: &str) {
        self.thumbnails.retain(|(fid, _), _| fid != flow_id);
    }
}

/// Compact thumbnail display for graph nodes
pub fn show_compact(ui: &mut egui::Ui, thumbnail: Option<&ThumbnailData>, size: egui::Vec2) {
    let (rect, _response) = ui.allocate_exact_size(size, egui::Sense::hover());

    if let Some(thumb) = thumbnail {
        // Draw thumbnail scaled to fit
        ui.painter().image(
            thumb.texture.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else {
        // Placeholder when no thumbnail available
        ui.painter().rect_filled(rect, 2.0, egui::Color32::from_gray(40));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No signal",
            egui::FontId::proportional(10.0),
            egui::Color32::from_gray(100),
        );
    }
}

/// Full thumbnail display for property inspector
pub fn show_full(ui: &mut egui::Ui, thumbnail: Option<&ThumbnailData>) {
    if let Some(thumb) = thumbnail {
        let aspect = thumb.width as f32 / thumb.height as f32;
        let max_width = ui.available_width().min(320.0);
        let size = egui::vec2(max_width, max_width / aspect);

        ui.image(&thumb.texture, size);

        ui.label(format!("{}×{}", thumb.width, thumb.height));
        ui.label(format!(
            "Updated {:.1}s ago",
            thumb.last_update.elapsed().as_secs_f32()
        ));
    } else {
        ui.label("No video signal");
    }
}
```

### 4. Compositor Editor Integration

The compositor editor can look up thumbnails by tracing upstream connections:

```rust
// In compositor_editor.rs

impl CompositorEditor {
    /// Find thumbnail for a compositor input by tracing upstream
    fn get_input_thumbnail(&self, input_idx: usize) -> Option<&ThumbnailData> {
        // Get the flow definition
        let flow = self.flows.get(&self.flow_id)?;

        // Find what's connected to this compositor input pad
        let mixer_input_pad = format!("sink_{}", input_idx);

        // Trace upstream to find a thumbnail block
        for connection in &flow.connections {
            if connection.to_element == self.mixer_element_id
                && connection.to_pad == mixer_input_pad
            {
                // Check if source is a thumbnail block, or trace further upstream
                if let Some(block) = flow.blocks.get(&connection.from_element) {
                    if block.block_type == "builtin.thumbnail" {
                        return self.thumbnail_store.get(&self.flow_id, &connection.from_element);
                    }
                }
                // Could recursively trace upstream to find thumbnail blocks
            }
        }

        None
    }

    /// Render input box with optional thumbnail background
    fn render_input_box(&self, ui: &mut egui::Ui, input: &InputBox, rect: egui::Rect) {
        // Try to get thumbnail for this input
        if let Some(thumb) = self.get_input_thumbnail(input.index) {
            // Draw thumbnail as background
            ui.painter().image(
                thumb.texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE.linear_multiply(0.8), // Slightly dim
            );
        }

        // Draw border and controls on top
        let stroke = if input.selected {
            egui::Stroke::new(2.0, egui::Color32::WHITE)
        } else {
            egui::Stroke::new(1.0, input.color)
        };
        ui.painter().rect_stroke(rect, 0.0, stroke);

        // Label
        ui.painter().text(
            rect.left_top() + egui::vec2(4.0, 4.0),
            egui::Align2::LEFT_TOP,
            &input.label,
            egui::FontId::proportional(12.0),
            egui::Color32::WHITE,
        );
    }
}
```

## Usage Patterns

### Pattern 1: Monitor compositor inputs

```
[Camera] → [Thumbnail] → [Compositor] → [Output]
[Graphics] → [Thumbnail] ↗
```

### Pattern 2: Transcoding preview

```
[File] → [Decoder] → [Thumbnail] → [Encoder] → [Output]
```

### Pattern 3: Multi-point monitoring

```
[Source] → [Thumbnail] → [Processing] → [Thumbnail] → [Output]
              ↓                            ↓
         Before preview              After preview
```

## Performance Considerations

- **Thumbnail size**: Default 160×90 keeps JPEG size ~5-10KB
- **Update interval**: 500ms default (2 fps) is sufficient for layout editing
- **Queue with leaky=downstream**: Drops frames if thumbnail processing is slow
- **JPEG encoding**: Hardware-accelerated if available (nvjpegenc on NVIDIA)
- **WebSocket**: Base64 adds ~33% overhead; could use binary frames for efficiency
- **Texture caching**: egui handles GPU texture upload efficiently

## Future Enhancements

1. **Binary WebSocket frames**: Avoid base64 encoding overhead
2. **Hardware JPEG encoding**: Use nvjpegenc when available
3. **Adaptive quality**: Lower quality/resolution under high load
4. **Thumbnail caching**: Persist last frame when pipeline stops
5. **Click-to-fullscreen**: Expand thumbnail to larger preview
6. **Frame scrubbing**: For file sources, scrub through timeline

## Related Files

- `backend/src/blocks/builtin/meter.rs` - Reference implementation for audio
- `frontend/src/meter.rs` - Reference for frontend data store
- `types/src/events.rs` - Event definitions
- `frontend/src/compositor_editor.rs` - Primary consumer for compositor use case
