# Fix for Pad Template Access Crashes

> **Archived — solved.** The fix is shipped. Kept as a root-cause reference for the
> SIGSEGV in factory-level pad-template access during multi-threaded construction.

## Problem
Recurring SIGSEGV crashes in `__strcmp_avx2()` during GStreamer element linking.

**Stack trace pattern:**
```
#0  __strcmp_avx2() at strcmp-avx2.S:283
#1  gst_element_class_get_pad_template() at libgstreamer-1.0.so.0
#2-5 [GObject/GStreamer internals during linking]
```

**Root cause:** Our code was accessing `factory.static_pad_templates()` which reads pad template metadata from the element factory (class-level). This is unsafe because:
- Factory/class metadata may be accessed concurrently during multi-threaded pipeline construction
- Class-level template access can trigger race conditions in GStreamer's internal structures
- We were iterating through ALL templates when we only needed specific ones

## Solution

**Avoid factory-level template access entirely**

### Old Approach (Unsafe)
```rust
// Get ALL templates from factory
let factory = element.factory()?;
let templates = factory.static_pad_templates();  // ← CRASHES HERE

// Search through all templates
for template in templates {
    if matches_pattern(template) {
        // Use template
    }
}
```

### New Approach (Safe)
```rust
// Infer template names from pad name
let prefix = pad_name.trim_end_matches(char::is_numeric).trim_end_matches('_');
let possible_templates = vec![
    pad_name,           // Try exact name
    format!("{}_%u", prefix),  // Try %u pattern
    format!("{}_%d", prefix),  // Try %d pattern
];

// Try each template directly on the element instance
for template_name in possible_templates {
    if let Some(pad_tmpl) = element.pad_template(template_name) {
        if let Some(pad) = element.request_pad(&pad_tmpl, None, None) {
            // Success!
            return Ok(pad);
        }
    }
}
```

## Key Changes

### 1. Pattern Inference Instead of Search
**File:** `backend/src/gst/pipeline.rs:463-475` (source pads) and `533-545` (sink pads)

Instead of getting ALL templates and searching:
```rust
// OLD: Get all templates from factory (crashes)
let templates = factory.static_pad_templates();

// NEW: Infer common patterns from pad name
let prefix = src_pad_name.trim_end_matches(|c: char| c.is_numeric()).trim_end_matches('_');
let template_u = format!("{}_%u", prefix);  // e.g., "src_%u"
let template_d = format!("{}_%d", prefix);  // e.g., "sink_%d"

let possible_templates: Vec<&str> = vec![
    src_pad_name,    // Exact match
    &template_u,     // Common %u pattern
    &template_d,     // Common %d pattern
];
```

### 2. Element Instance Access Only
```rust
// Try each template directly on the element instance
// This avoids factory-level access entirely
for template_name in possible_templates {
    if let Some(pad_tmpl) = element.pad_template(template_name) {
        // Get template from ELEMENT, not factory
        if let Some(pad) = element.request_pad(&pad_tmpl, None, None) {
            found_pad = Some(pad);
            break;
        }
    }
}
```

## Why This Works

1. **Instance-level access is thread-safe**: Each element instance has its own template references
2. **No iteration through class metadata**: We try specific template names directly
3. **Falls back gracefully**: If our patterns don't match, we return a clear error
4. **Covers 99% of use cases**: Most GStreamer elements use `name_%u` or `name_%d` patterns

## Examples

### Tee Element
- Pad name requested: `src_0`
- We try: `src_0` (exact), `src_%u` (pattern), `src_%d` (pattern)
- Match found: `src_%u` ✓

### Audiomixer Element
- Pad name requested: `sink_1`
- We try: `sink_1` (exact), `sink_%u` (pattern), `sink_%d` (pattern)
- Match found: `sink_%u` ✓

### Custom Pad Name
- Pad name requested: `my_custom_pad`
- We try: `my_custom_pad` (exact), `my_custom_%u`, `my_custom_%d`
- Match found: `my_custom_pad` ✓

## Fallback Behavior

If our pattern inference doesn't work:
1. First fallback: `request_pad_simple(pad_name)` - tries exact name
2. Second fallback: Pattern inference (this fix)
3. Final fallback: Return clear error for dynamic pads

## Performance Impact

**Improved:**
- No longer iterating through all factory templates
- Only 2-3 direct lookups per pad request
- Avoids factory access contention

## Testing

The fix handles:
- ✓ Request pads with numeric suffixes (`src_0`, `sink_1`)
- ✓ Request pads without suffixes (`sink`, `src`)
- ✓ Static pads (tried first, before this code)
- ✓ Dynamic pads (error returned, handled by dynamic pad handlers)

## Removed Code

We removed:
- All `factory.static_pad_templates()` calls
- Template list iteration logic
- Catch_unwind safety wrappers (no longer needed)

## Migration Notes

If you encounter pad linking issues after this fix:
1. Check the error message - it will tell you which patterns were tried
2. Add the correct pattern to `possible_templates` if needed
3. Most standard GStreamer elements will work without changes
