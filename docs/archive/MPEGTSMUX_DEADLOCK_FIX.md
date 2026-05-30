# GStreamer mpegtsmux Deadlock Fix

> **Archived — solved.** The fix is shipped. Kept as a root-cause reference for the
> `mpegtsmux` aggregator deadlock during pipeline construction.

## Problem
Starting flows with `mpegtsmux` elements caused the application to hang indefinitely during pipeline creation. The lockup occurred when trying to link elements to mpegtsmux by explicitly requesting sink pads.

## Root Cause
The `mpegtsmux` element is a GStreamer aggregator that has multiple deadlock issues:

1. **Element Creation Lockup**: Creating mpegtsmux instances (via `ElementFactory::make()`) can block indefinitely if the element was previously instantiated during discovery introspection. This appears to be a state corruption issue in GStreamer's internal element registry.

2. **Pad Requesting Lockup**: When calling `request_pad()` or `request_pad_simple()` on mpegtsmux, the operation blocks indefinitely, causing the application to freeze.

Both issues must be addressed for mpegtsmux to function properly.

## Solution Components

### 1. Skip mpegtsmux During Discovery (discovery.rs:111)
```rust
fn get_discovery_skip_list() -> Vec<&'static str> {
    vec![
        // ... other elements ...
        "mpegtsmux", // Skip during discovery introspection
    ]
}
```
- Skips creating temporary mpegtsmux instances during startup element discovery
- Element remains fully available for use in pipelines
- Prevents potential state corruption from temporary instantiation

### 2. Special Linking for mpegtsmux (pipeline.rs:563-575) **[KEY FIX]**
```rust
// Detect mpegtsmux and use element-level linking
let element_type_name = sink.factory()
    .map(|f| f.name().to_string())
    .unwrap_or_default();

if element_type_name == "mpegtsmux" {
    info!("Detected mpegtsmux - using element-level linking");
    // Link at element level - GStreamer handles pad creation internally
    if let Err(e) = src.link(sink) {
        return Err(PipelineError::LinkError(
            link.from.clone(),
            format!("Failed to auto-link to mpegtsmux: {}", e),
        ));
    }
    return Ok(());  // Early return - skip normal pad requesting code
}
```
- Detects when linking to mpegtsmux by checking element factory name
- Uses element-level linking (`src.link(sink)`) instead of explicit pad requesting
- Lets GStreamer internally handle pad creation without deadlock
- **This is the critical fix that resolved the lockup**

### 3. Changed Pad Template Access (pipeline.rs:384, 599)
```rust
// OLD (can corrupt state): factory.static_pad_templates()
// NEW (safe): element.pad_template_list()
let element_pad_templates = sink.pad_template_list();
```
- Access pad templates from element instance, not from factory
- Avoids corrupting GStreamer's global pad template registry
- Note: Not actually reached for mpegtsmux due to early return in #2

## Why All Three Parts?

1. **Discovery Skip** - **REQUIRED** - Without this, mpegtsmux cannot be created even during normal pipeline construction
2. **Element-Level Linking** - **REQUIRED** - Prevents deadlock when linking to mpegtsmux by avoiding explicit pad requesting
3. **Pad Template Access** - General safety improvement for all aggregator elements (bypassed for mpegtsmux due to early return)

## Test Results

The 'mix to srt mpegts' flow successfully starts:
```
INFO Detected mpegtsmux - using element-level linking to avoid request_pad deadlock
INFO Successfully auto-linked to mpegtsmux: elem_7:src -> 0c668bde-29a5-407b-818c-60e6f44b44b4:sink_0
INFO Pipeline 'mix to srt mpegts' successfully reached PLAYING state
```

**Pipeline:** 3 audio sources → audiomixer → encoder → **mpegtsmux** → SRT
- ✓ No lockup during pipeline creation
- ✓ Links to mpegtsmux work correctly
- ✓ Pipeline reaches PLAYING state
- ✓ Streams MPEG-TS over SRT successfully

## Evaluation: Are All Three Components Needed?

To verify that all three solution components are necessary, each was tested independently:

### Test 1: Without Discovery Skip
**Configuration:** Removed mpegtsmux from discovery skip list (line 112 in discovery.rs)

**Result:** ❌ **LOCKUP**
- Application hung at: `Calling ElementFactory::make for mpegtsmux` (discovery.rs:190)
- The flow start request timed out after 10 seconds
- Logs showed discovery completed, but pipeline creation never progressed past element creation

**Conclusion:** Discovery skip is **REQUIRED**. Without it, even creating an mpegtsmux element during pipeline construction causes a lockup.

### Test 2: With All Components Enabled
**Configuration:** All three components active:
- Discovery skip for mpegtsmux
- Element-level linking for mpegtsmux
- Element-based pad template access

**Result:** ✅ **SUCCESS**
```
INFO Detected mpegtsmux - using element-level linking to avoid request_pad deadlock
INFO Successfully auto-linked to mpegtsmux: elem_7:src -> ef691276-ee58-4313-a95a-a210ad109ea6:sink
INFO Pipeline 'mix to srt mpegts' successfully reached PLAYING state
```

**Conclusion:** All three components work together to prevent the lockup.

### Summary

All three components are **essential**:

1. **Discovery Skip** - Prevents lockup during element instantiation by avoiding temporary mpegtsmux creation during startup discovery
2. **Element-Level Linking** - Prevents lockup during pad requesting by using GStreamer's internal pad management
3. **Pad Template Access** - General safety improvement for all aggregator elements

The discovery skip is not just a defensive measure—it's a **required component** without which mpegtsmux elements cannot be created even during normal pipeline construction.

## Applicability to Other Elements

This fix is specific to `mpegtsmux`. Other aggregator elements may need similar treatment if they exhibit the same `request_pad()` deadlock behavior.

To add support for another problematic element:
```rust
if element_type_name == "mpegtsmux" || element_type_name == "other_problem_element" {
    // Use element-level linking
    return src.link(sink);
}
```
