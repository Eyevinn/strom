# Compositor Layout Editor - Implementation Summary

## Overview
A visual, interactive layout editor for the `glcompositor` block that allows drag-and-drop repositioning and resizing of input video sources in real-time while the pipeline is running.

## What's Implemented

### 1. Core Module (`frontend/src/compositor_editor.rs`)
- **CompositorEditor** struct with complete UI logic
- **InputBox** struct representing each compositor input
- **ResizeHandle** enum for 8-point resizing (corners + edges)

### 2. Features Implemented
✅ Canvas-based visual representation of compositor layout
✅ Draggable input boxes with snap-to-grid option
✅ 8-point resize handles (corners + edges)
✅ Real-time API updates to running pipeline
✅ Optimistic UI updates with error handling
✅ Property panel for fine-tuning (alpha, zorder, sizing-policy)
✅ Color-coded input boxes by index
✅ Z-order visual indication
✅ Grid overlay (optional)
✅ Zoom controls
✅ Live update toggle

### 3. API Integration (`frontend/src/api.rs`)
Added two new methods to `ApiClient`:
- `get_pad_properties()` - Fetch current mixer pad properties
- `update_pad_property()` - Update a single pad property

### 4. App Integration (`frontend/src/app.rs`)
- Added `compositor_editor: Option<CompositorEditor>` field
- Added local storage helper functions (WASM + native)
- Integrated compositor editor import

## Integration Status

All integration is complete:

1. **app.rs** - Handles compositor editor lifecycle:
   - Opens editor when `open_compositor_editor` signal is detected in local storage
   - Extracts block properties (output_width, output_height, num_inputs)
   - Creates and displays the editor window
   - Closes editor when window is closed

2. **graph.rs** - Double-click handler:
   - Double-clicking on a `builtin.glcompositor` block opens the layout editor

3. **api.rs** - Backend communication:
   - `get_pad_properties()` - Fetch current mixer pad properties
   - `update_pad_property()` - Update pad properties in real-time

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
- **Close** to return to flow graph view

### Real-Time Updates
All changes are applied immediately to the running pipeline via:
- `PATCH /api/flows/{flow_id}/elements/{block_id}:mixer/pads/sink_{i}/properties`

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

### Local Storage Bridge
Due to WASM async limitations, the editor uses local storage as a message bus:
- `compositor_props_{flow_id}_{pad_name}` - Loaded properties
- `compositor_update_success_{index}_{prop}` - Update succeeded
- `compositor_update_error_{index}_{prop}` - Update error message
- `open_compositor_editor` - Signal to open editor
- `close_compositor_editor` - Signal to close editor

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
12. Close editor - verify return to graph view

### Error Handling Test
1. Open editor on stopped flow - should show error
2. Update property with invalid value - should show error and revert
3. Delete block while editor open - editor should close gracefully

## Future Enhancements
- [ ] Preview thumbnails of actual video in input boxes
- [ ] Undo/redo for layout changes
- [ ] Layout presets (picture-in-picture, split-screen, etc.)
- [ ] Keyboard shortcuts for precision positioning
- [ ] Copy/paste layout between compositors
- [ ] Export/import layout as JSON
- [ ] Animation/transitions between layouts

## Files Created/Modified

### New Files
- `frontend/src/compositor_editor.rs` - Complete compositor editor implementation

### Modified Files
- `frontend/src/lib.rs` - Added compositor_editor module
- `frontend/src/app.rs` - Added import, local storage helpers, compositor_editor field, open signal handling, rendering
- `frontend/src/api.rs` - Added get_pad_properties() and update_pad_property()
- `frontend/src/graph.rs` - Added double-click handler for glcompositor blocks
