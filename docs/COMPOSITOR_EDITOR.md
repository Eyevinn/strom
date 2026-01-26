# Compositor Layout Editor - Implementation Summary

> **WIP Notice**: Scene transitions and Live View mode are work in progress. There are known bugs around mixing UI live view settings and transitions - these features may not work correctly together in all cases.

## Overview
A visual, interactive layout editor for the `glcompositor` block that allows drag-and-drop repositioning and resizing of input video sources in real-time while the pipeline is running.

## What's Implemented

### 1. Core Module (`frontend/src/compositor_editor.rs`)
- **CompositorEditor** struct with complete UI logic
- **InputBox** struct representing each compositor input
- **ResizeHandle** enum for 8-point resizing (corners + edges)

### 2. Features Implemented

#### Basic Editor
- Canvas-based visual representation of compositor layout
- Draggable input boxes with snap-to-grid option
- 8-point resize handles (corners + edges)
- Real-time API updates to running pipeline
- Optimistic UI updates with error handling
- Property panel for fine-tuning (alpha, zorder, sizing-policy)
- Color-coded input boxes by index
- Z-order visual indication
- Grid overlay (optional)
- Zoom controls
- Live update toggle

#### Scene Transitions (NEW)
- **Cut** - Instant switch between inputs
- **Fade** - Cross-fade via alpha blending
- **Slide** - Slide new input in from left/right/up/down (old stays in place)
- **Push** - Push transition where both inputs move together
- **Dip-to-black** - Fade out to black, then fade in new source
- Configurable duration (100ms - 2000ms)
- Cubic easing for smooth animations
- Uses GStreamer Controller API with InterpolationControlSource

#### Live View Mode (NEW)
- Toggle between Edit mode and Live View mode
- Shows real-time video thumbnails on input boxes
- Visual feedback during transitions
- Thumbnails fetched via API endpoint

#### Thumbnail Capture (NEW)
- Poll-based frame capture from compositor inputs
- Supports multiple video formats: RGB, RGBA, BGR, BGRA, I420, YV12, NV12, YUY2, UYVY
- Configurable thumbnail size (default 320x180)
- JPEG encoding with configurable quality

#### Keyboard Shortcuts (NEW)
- **R** - Reset selected input to fullscreen
- **1-9** - Quick-select input by number
- **Delete/Backspace** - Hide selected input (set alpha to 0)

### 3. API Integration (`frontend/src/api.rs`)
- `get_pad_properties()` - Fetch current mixer pad properties
- `update_pad_property()` - Update a single pad property
- `get_compositor_thumbnail()` - Fetch thumbnail for compositor input
- `animate_compositor_input()` - Trigger animated layout change
- `start_compositor_transition()` - Start scene transition between inputs

### 4. Backend Modules

#### Transitions (`backend/src/gst/transitions.rs`)
- `TransitionController` - Manages active transitions per pipeline
- `TransitionType` enum - All supported transition types
- Uses GStreamer `InterpolationControlSource` for smooth property animation
- Keyframe-based animation with cubic easing

#### Thumbnail Capture (`backend/src/gst/thumbnail.rs`, `video_frame.rs`)
- `capture_frame_as_jpeg()` - Capture single frame via pad probe
- `video_frame` module - Reusable YUV/RGB conversion functions
- `ThumbnailConfig` - Configurable dimensions and quality

### 5. API Endpoints

#### Thumbnail Endpoint
```
GET /api/flows/{flow_id}/compositor/{block_id}/thumbnail/{input_idx}
    ?width=320&height=180&quality=75
```
Returns JPEG image bytes.

#### Transition Endpoint
```
POST /api/flows/{flow_id}/compositor/{block_id}/transition
{
    "from_input": 0,
    "to_input": 1,
    "transition_type": "fade",
    "duration_ms": 500
}
```

#### Animation Endpoint
```
POST /api/flows/{flow_id}/compositor/{block_id}/animate
{
    "input": 0,
    "xpos": 100,
    "ypos": 100,
    "width": 800,
    "height": 450,
    "alpha": 1.0,
    "duration_ms": 300
}
```

## Usage

### Opening the Editor
1. Create a flow with a `builtin.glcompositor` block
2. Start the flow
3. Double-click on the compositor block to open the layout editor

### Using the Editor
- **Drag boxes** to reposition inputs
- **Drag resize handles** to change input sizes
- **Click a box** to select it and show properties panel
- **Adjust properties** in the side panel (alpha, zorder, etc.)
- **Toggle grid snapping** for precision alignment
- **Zoom in/out** to work with detail or overview
- **Press R** to reset selected input to fullscreen
- **Close** to return to flow graph view

### Using Transitions
1. Select transition type from dropdown (Cut, Fade, Slide, Push, Dip-to-black)
2. Set transition duration
3. Click on an input to transition to it
4. The transition animates automatically

### Live View Mode
1. Click "Live View" button to toggle mode
2. Thumbnails appear on input boxes showing actual video
3. Click "Edit" to return to layout editing mode

## Technical Details

### Element ID Format
Compositor blocks create internal elements with IDs:
- Mixer element: `{block_id}:mixer` (e.g., `b0:mixer`)
- Pads: `sink_0`, `sink_1`, ..., `sink_N`

### Properties Updated
- `xpos` - X position in pixels (Int)
- `ypos` - Y position in pixels (Int)
- `width` - Width in pixels (Int)
- `height` - Height in pixels (Int)
- `alpha` - Transparency 0.0-1.0 (Float)
- `zorder` - Layer order 0-15 (UInt)
- `sizing-policy` - "none" or "keep-aspect-ratio" (String)

### Transition Implementation
Transitions use GStreamer's Controller subsystem:
1. Create `InterpolationControlSource` for each animated property
2. Set interpolation mode to `CubicMonotonic` for smooth easing
3. Add keyframes at start time and end time
4. Bind control source to pad properties
5. GStreamer automatically interpolates values during playback

### Local Storage Bridge
Due to WASM async limitations, the editor uses local storage as a message bus:
- `compositor_props_{flow_id}_{pad_name}` - Loaded properties
- `compositor_update_success_{index}_{prop}` - Update succeeded
- `compositor_update_error_{index}_{prop}` - Update error message
- `compositor_thumb_{flow_id}_{input}` - Thumbnail data (base64)
- `open_compositor_editor` - Signal to open editor
- `close_compositor_editor` - Signal to close editor

## Known Issues (WIP)

- Mixing Live View settings with transitions may cause unexpected behavior
- Rapid transition triggering can cause animation glitches
- Thumbnail fetch may timeout if pipeline is under heavy load

## Testing

### Manual Test Plan
1. Create flow with videotestsrc -> glcompositor -> autovideosink
2. Add 2-4 inputs to compositor
3. Start flow
4. Open compositor editor
5. Verify all inputs appear on canvas
6. Drag an input - verify position updates in real-time
7. Resize an input - verify size updates in real-time
8. Adjust alpha slider - verify transparency changes
9. Change zorder - verify layering changes
10. Toggle grid snapping - verify snapping behavior
11. Zoom in/out - verify canvas scaling
12. Test transitions between inputs
13. Toggle Live View and verify thumbnails appear
14. Press R to reset input to fullscreen
15. Close editor - verify return to graph view

### Error Handling Test
1. Open editor on stopped flow - should show error
2. Update property with invalid value - should show error and revert
3. Delete block while editor open - editor should close gracefully

## Future Enhancements
- [ ] Undo/redo for layout changes
- [ ] Layout presets (picture-in-picture, split-screen, etc.)
- [ ] Copy/paste layout between compositors
- [ ] Export/import layout as JSON
- [ ] Streaming thumbnails via WebSocket (see `docs/design/video-thumbnail-block.md`)

## Files

### Backend
- `backend/src/gst/transitions.rs` - Scene transition controller
- `backend/src/gst/thumbnail.rs` - Frame capture for thumbnails
- `backend/src/gst/video_frame.rs` - Video format conversion utilities
- `backend/src/api/flows.rs` - API endpoints for transitions and thumbnails

### Frontend
- `frontend/src/compositor_editor.rs` - Complete compositor editor implementation
- `frontend/src/api.rs` - API client methods
- `frontend/src/app.rs` - Editor lifecycle and Live View mode
