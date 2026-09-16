use egui::Context;
use strom_types::PropertyValue;

use super::*;

impl CompositorEditor {
    /// Load current properties from the backend.
    pub fn load_properties(&mut self, ctx: &Context) {
        let flow_id = self.flow_id;
        let mixer_element_id = self.mixer_element_id.clone();
        let api = self.api.clone();
        let ctx = ctx.clone();

        // Load properties for each input
        for input in &self.inputs {
            let pad_name = input.pad_name.clone();
            let api = api.clone();
            let ctx = ctx.clone();
            let mixer_element_id = mixer_element_id.clone();

            crate::app::spawn_task(async move {
                match api
                    .get_pad_properties(&flow_id.to_string(), &mixer_element_id, &pad_name)
                    .await
                {
                    Ok(props) => {
                        // Store properties in local storage for the UI loop to pick up
                        let key = format!("compositor_props_{}_{}", flow_id, pad_name);
                        if let Ok(json) = serde_json::to_string(&props) {
                            crate::app::set_local_storage(&key, &json);
                        }
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        tracing::error!("Failed to load pad properties for {}: {}", pad_name, e);
                    }
                }
            });
        }
    }

    /// Check for loaded properties and update inputs.
    pub(super) fn check_loaded_properties(&mut self) {
        for input in &mut self.inputs {
            let key = format!("compositor_props_{}_{}", self.flow_id, input.pad_name);
            if let Some(json) = crate::app::get_local_storage(&key) {
                if let Ok(props) =
                    serde_json::from_str::<std::collections::HashMap<String, PropertyValue>>(&json)
                {
                    // Update input box with loaded properties
                    if let Some(PropertyValue::Int(v)) = props.get("xpos") {
                        input.xpos = *v as i32;
                    }
                    if let Some(PropertyValue::Int(v)) = props.get("ypos") {
                        input.ypos = *v as i32;
                    }
                    if let Some(PropertyValue::Int(v)) = props.get("width") {
                        input.width = *v as i32;
                    }
                    if let Some(PropertyValue::Int(v)) = props.get("height") {
                        input.height = *v as i32;
                    }
                    if let Some(PropertyValue::Float(v)) = props.get("alpha") {
                        input.alpha = *v;
                    }
                    if let Some(PropertyValue::UInt(v)) = props.get("zorder") {
                        input.zorder = *v as u32;
                    }
                    if let Some(PropertyValue::String(v)) = props.get("sizing-policy") {
                        input.sizing_policy = v.clone();
                    }

                    // Clear the storage key
                    crate::app::remove_local_storage(&key);

                    self.status = "Properties loaded".to_string();
                }
            }
        }
    }

    /// Refresh thumbnails for all inputs.
    /// Called periodically to update the thumbnail images.
    pub(super) fn refresh_thumbnails(&mut self, ctx: &Context) {
        if !self.thumbnails_enabled {
            return;
        }

        let now = instant::Instant::now();
        let refresh_interval = std::time::Duration::from_millis(self.thumbnail_refresh_ms);

        for input in &self.inputs {
            let idx = input.input_index;

            // Skip if already loading
            if self.thumbnail_loading.contains(&idx) {
                continue;
            }

            // Skip if recently fetched
            if let Some(last_fetch) = self.thumbnail_fetch_times.get(&idx) {
                if now.duration_since(*last_fetch) < refresh_interval {
                    continue;
                }
            }

            // Mark as loading and update fetch time
            self.thumbnail_loading.insert(idx);
            self.thumbnail_fetch_times.insert(idx, now);

            // Spawn async fetch
            let flow_id = self.flow_id;
            let block_id = self.block_id.clone();
            let api = self.api.clone();
            let ctx = ctx.clone();

            tracing::debug!(
                "Fetching thumbnail for flow={} block={} input={}",
                flow_id,
                block_id,
                idx
            );

            crate::app::spawn_task(async move {
                match api
                    .get_block_thumbnail(&flow_id.to_string(), &block_id, idx)
                    .await
                {
                    Ok(jpeg_bytes) => {
                        tracing::debug!(
                            "Got thumbnail {} bytes for input {}",
                            jpeg_bytes.len(),
                            idx
                        );
                        // Store bytes in local storage for the UI thread to pick up
                        let key = format!("compositor_thumb_{}_{}", flow_id, idx);
                        // Use base64 to store binary data
                        use base64::Engine;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_bytes);
                        crate::app::set_local_storage(&key, &b64);
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        tracing::debug!("Failed to fetch thumbnail for input {}: {}", idx, e);
                        // Store error marker so the loading flag gets cleared
                        let key = format!("compositor_thumb_err_{}_{}", flow_id, idx);
                        crate::app::set_local_storage(&key, "error");
                        ctx.request_repaint();
                    }
                }
            });
        }
    }

    /// Check for loaded thumbnails and update textures.
    pub(super) fn check_loaded_thumbnails(&mut self, ctx: &Context) {
        let num_inputs = self.inputs.len();
        for idx in 0..num_inputs {
            // Check for error marker first
            let err_key = format!("compositor_thumb_err_{}_{}", self.flow_id, idx);
            if crate::app::get_local_storage(&err_key).is_some() {
                crate::app::remove_local_storage(&err_key);
                self.thumbnail_loading.remove(&idx);
                continue;
            }

            let key = format!("compositor_thumb_{}_{}", self.flow_id, idx);
            if let Some(b64) = crate::app::get_local_storage(&key) {
                tracing::debug!(
                    "Found thumbnail in storage for input {}, {} bytes b64",
                    idx,
                    b64.len()
                );
                // Clear the loading flag
                self.thumbnail_loading.remove(&idx);

                // Decode base64
                use base64::Engine;
                match base64::engine::general_purpose::STANDARD.decode(&b64) {
                    Ok(jpeg_bytes) => {
                        tracing::debug!(
                            "Decoded {} JPEG bytes for input {}, header: {:02X} {:02X} {:02X}",
                            jpeg_bytes.len(),
                            idx,
                            jpeg_bytes.first().copied().unwrap_or(0),
                            jpeg_bytes.get(1).copied().unwrap_or(0),
                            jpeg_bytes.get(2).copied().unwrap_or(0)
                        );
                        // Decode JPEG to image - use explicit format to avoid guess issues
                        match image::load_from_memory_with_format(
                            &jpeg_bytes,
                            image::ImageFormat::Jpeg,
                        ) {
                            Ok(img) => {
                                let rgba = img.to_rgba8();
                                let size = [rgba.width() as usize, rgba.height() as usize];
                                tracing::debug!(
                                    "Loaded image {}x{} for input {}",
                                    size[0],
                                    size[1],
                                    idx
                                );
                                let color_image =
                                    egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());

                                // Create or update texture
                                let texture_name = format!("compositor_thumb_{}", idx);
                                let texture = ctx.load_texture(
                                    texture_name,
                                    color_image,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.thumbnails.insert(idx, texture);
                                tracing::debug!("Created texture for input {}", idx);
                            }
                            Err(e) => {
                                tracing::warn!("Failed to decode JPEG for input {}: {}", idx, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to decode base64 for input {}: {}", idx, e);
                    }
                }

                // Clear the storage key
                crate::app::remove_local_storage(&key);
            }
        }
    }

    /// Update a pad property via API.
    pub(super) fn update_pad_property(
        &mut self,
        ctx: &Context,
        input_index: usize,
        property_name: &str,
        value: PropertyValue,
    ) {
        if !self.live_updates {
            tracing::debug!(
                "Live updates disabled, skipping API call for {}={:?}",
                property_name,
                value
            );
            return;
        }

        let flow_id = self.flow_id;
        let mixer_element_id = self.mixer_element_id.clone();
        let pad_name = self.inputs[input_index].pad_name.clone();
        let api = self.api.clone();
        let ctx = ctx.clone();
        let property_name = property_name.to_string();

        tracing::info!(
            "Updating compositor pad property: flow={} element={} pad={} property={}={:?}",
            flow_id,
            mixer_element_id,
            pad_name,
            property_name,
            value
        );

        self.inputs[input_index].pending_update = true;
        self.status = format!("Updating {}...", property_name);

        crate::app::spawn_task(async move {
            match api
                .update_pad_property(
                    &flow_id.to_string(),
                    &mixer_element_id,
                    &pad_name,
                    &property_name,
                    value.clone(),
                )
                .await
            {
                Ok(_) => {
                    tracing::info!(
                        "Compositor pad property updated: {}={:?}",
                        property_name,
                        value
                    );
                    let key = format!(
                        "compositor_update_success_{}_{}",
                        input_index, property_name
                    );
                    crate::app::set_local_storage(&key, "1");
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to update compositor pad property {}: {}",
                        property_name,
                        e
                    );
                    let key = format!("compositor_update_error_{}_{}", input_index, property_name);
                    crate::app::set_local_storage(&key, &e.to_string());
                }
            }
            ctx.request_repaint();
        });
    }

    /// Apply all properties for all inputs (used when live updates is off).
    pub(super) fn apply_all_properties(&mut self, ctx: &Context) {
        // Temporarily enable live updates to send all properties
        let was_live = self.live_updates;
        self.live_updates = true;

        for idx in 0..self.inputs.len() {
            let input = &self.inputs[idx];
            let xpos = input.xpos;
            let ypos = input.ypos;
            let width = input.width;
            let height = input.height;
            let alpha = input.alpha;
            let zorder = input.zorder;
            let sizing_policy = input.sizing_policy.clone();

            self.update_pad_property(ctx, idx, "xpos", PropertyValue::Int(xpos as i64));
            self.update_pad_property(ctx, idx, "ypos", PropertyValue::Int(ypos as i64));
            self.update_pad_property(ctx, idx, "width", PropertyValue::Int(width as i64));
            self.update_pad_property(ctx, idx, "height", PropertyValue::Int(height as i64));
            self.update_pad_property(ctx, idx, "alpha", PropertyValue::Float(alpha));
            self.update_pad_property(ctx, idx, "zorder", PropertyValue::UInt(zorder as u64));
            self.update_pad_property(
                ctx,
                idx,
                "sizing-policy",
                PropertyValue::String(sizing_policy),
            );
        }

        // Restore live updates setting
        self.live_updates = was_live;
        self.status = "Layout applied".to_string();
    }

    /// Check for update results.
    pub(super) fn check_update_results(&mut self) {
        for input in &mut self.inputs {
            let success_key = format!("compositor_update_success_{}_", input.input_index);
            let error_key = format!("compositor_update_error_{}_", input.input_index);

            // Check for success
            if crate::app::get_local_storage(&success_key).is_some() {
                input.pending_update = false;
                input.last_error = None;
                crate::app::remove_local_storage(&success_key);
            }

            // Check for error
            if let Some(err) = crate::app::get_local_storage(&error_key) {
                input.pending_update = false;
                input.last_error = Some(err);
                crate::app::remove_local_storage(&error_key);
            }
        }
    }

    /// Trigger a transition between inputs.
    ///
    /// Returns true if the request was dispatched. The from/to pair is swapped and
    /// the direction inverted optimistically, so the controls track the take without
    /// waiting for the round trip. A failed request queues a rollback that
    /// `check_transition_status` applies on the next frame.
    pub(super) fn trigger_transition(&mut self, ctx: &Context) -> bool {
        if self.transition_from == self.transition_to {
            return false;
        }

        let flow_id = self.flow_id;
        let block_id = self.block_id.clone();
        let from_input = self.transition_from;
        let to_input = self.transition_to;
        let transition_type = self.transition_type.clone();
        let duration_ms = self.transition_duration_ms;
        let api = self.api.clone();
        let ctx = ctx.clone();

        self.transition_status = Some(format!(
            "{}...",
            if transition_type == "cut" {
                "Cutting"
            } else {
                "Transitioning"
            }
        ));

        // Swap from/to immediately so "From" shows what's now live. The request is
        // expected to succeed; the error arm below undoes this.
        self.transition_from = to_input;
        self.transition_to = from_input;
        self.transition_type = invert_transition_direction(&transition_type);

        tracing::info!(
            "Triggering {} transition: {} -> {} ({}ms)",
            transition_type,
            from_input,
            to_input,
            duration_ms
        );

        crate::app::spawn_task(async move {
            match api
                .trigger_transition(
                    &flow_id.to_string(),
                    &block_id,
                    from_input,
                    to_input,
                    &transition_type,
                    duration_ms,
                )
                .await
            {
                Ok(_) => {
                    tracing::info!("Transition triggered successfully");
                    crate::app::set_local_storage(
                        &transition_status_key(&block_id),
                        &format!(
                            "{} {} -> {}",
                            egui_phosphor::regular::CHECK,
                            from_input,
                            to_input
                        ),
                    );
                }
                Err(e) => {
                    tracing::error!("Transition failed: {}", e);
                    crate::app::set_local_storage(
                        &transition_status_key(&block_id),
                        &format!("{} {}", egui_phosphor::regular::X, e),
                    );
                    // Nothing went to air, so the optimistic swap has to come back.
                    // The payload is what was staged before the swap; the UI loop
                    // restores it in `check_transition_status`.
                    crate::app::set_local_storage(
                        &transition_rollback_key(&block_id),
                        &format!("{}|{}|{}", from_input, to_input, transition_type),
                    );
                }
            }
            ctx.request_repaint();
        });

        true
    }

    /// Check for transition status updates, and undo the optimistic swap for any
    /// transition the backend rejected since the last frame.
    pub(super) fn check_transition_status(&mut self) {
        let status_key = transition_status_key(&self.block_id);
        if let Some(status) = crate::app::get_local_storage(&status_key) {
            self.transition_status = Some(status);
            crate::app::remove_local_storage(&status_key);
        }

        let rollback_key = transition_rollback_key(&self.block_id);
        let Some(rollback) = crate::app::get_local_storage(&rollback_key) else {
            return;
        };
        crate::app::remove_local_storage(&rollback_key);

        let Some((from_input, to_input, transition_type)) = parse_transition_rollback(&rollback)
        else {
            tracing::warn!("Ignoring malformed transition rollback: {}", rollback);
            return;
        };

        // Only undo a swap that is still standing. Anything the user changed while
        // the request was in flight wins, so a late rollback never overwrites a fresh
        // selection. The pair and the direction are guarded separately because they
        // are edited separately.
        if self.transition_from == to_input && self.transition_to == from_input {
            self.transition_from = from_input;
            self.transition_to = to_input;
        }
        if self.transition_type == invert_transition_direction(&transition_type) {
            self.transition_type = transition_type;
        }
    }
}

/// Local storage key for the human-readable status of the last transition.
fn transition_status_key(block_id: &str) -> String {
    format!("transition_status_{}", block_id)
}

/// Local storage key for a failed transition awaiting its optimistic swap being undone.
fn transition_rollback_key(block_id: &str) -> String {
    format!("transition_rollback_{}", block_id)
}

/// Parse a `from|to|type` rollback payload written by a failed transition. The
/// triple is the state as it was staged, before the optimistic swap.
fn parse_transition_rollback(value: &str) -> Option<(usize, usize, String)> {
    let mut parts = value.splitn(3, '|');
    let from_input = parts.next()?.parse().ok()?;
    let to_input = parts.next()?.parse().ok()?;
    let transition_type = parts.next()?.to_string();
    Some((from_input, to_input, transition_type))
}

/// Invert a slide/push direction so repeated takes move back and forth.
///
/// Every mapping is its own inverse, so applying this twice is the identity — which
/// is what lets the rollback path reuse it to undo an inversion.
fn invert_transition_direction(transition_type: &str) -> String {
    match transition_type {
        "slide_left" => "slide_right",
        "slide_right" => "slide_left",
        "slide_up" => "slide_down",
        "slide_down" => "slide_up",
        "push_left" => "push_right",
        "push_right" => "push_left",
        "push_up" => "push_down",
        "push_down" => "push_up",
        other => other,
    }
    .to_string()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::api::ApiClient;

    fn editor(block_id: &str) -> CompositorEditor {
        CompositorEditor::new(
            uuid::Uuid::nil(),
            block_id.to_string(),
            1920,
            1080,
            4,
            ApiClient::new_with_auth("http://127.0.0.1:1", None),
        )
    }

    /// The optimistic swap is the point of the eager path, so it must still land
    /// on dispatch without waiting for the response.
    #[tokio::test]
    async fn trigger_transition_swaps_optimistically() {
        let mut editor = editor("take-optimistic");
        editor.transition_type = "slide_left".to_string();

        assert!(editor.trigger_transition(&Context::default()));

        assert_eq!(editor.transition_from, 1);
        assert_eq!(editor.transition_to, 0);
        assert_eq!(editor.transition_type, "slide_right");
    }

    /// A take that never went to air must leave the controls where they started,
    /// or the next take runs the wrong way.
    #[test]
    fn failed_transition_rolls_the_swap_back() {
        let mut editor = editor("take-failed");
        // State as trigger_transition leaves it after the optimistic swap of 0 -> 1.
        editor.transition_from = 1;
        editor.transition_to = 0;
        editor.transition_type = "slide_right".to_string();
        crate::app::set_local_storage(&transition_rollback_key("take-failed"), "0|1|slide_left");

        editor.check_transition_status();

        assert_eq!(editor.transition_from, 0);
        assert_eq!(editor.transition_to, 1);
        assert_eq!(editor.transition_type, "slide_left");
        assert!(crate::app::get_local_storage(&transition_rollback_key("take-failed")).is_none());
    }

    /// End-to-end over a real failed request: the optimistic swap lands, the error
    /// arm queues the rollback, and the next frame undoes it. Guards the error-arm
    /// write, which the hand-seeded tests above cannot.
    #[tokio::test]
    async fn a_rejected_request_rolls_the_swap_back() {
        let mut editor = editor("take-rejected");
        editor.transition_type = "slide_left".to_string();

        assert!(editor.trigger_transition(&Context::default()));
        assert_eq!(editor.transition_from, 1);

        // Nothing listens on port 1, so the POST is refused promptly.
        let key = transition_rollback_key("take-rejected");
        for _ in 0..200 {
            if crate::app::get_local_storage(&key).is_some() {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        editor.check_transition_status();

        assert_eq!(editor.transition_from, 0);
        assert_eq!(editor.transition_to, 1);
        assert_eq!(editor.transition_type, "slide_left");
    }

    /// A rollback that arrives after the operator has re-staged must not overwrite
    /// the new selection.
    #[test]
    fn rollback_does_not_overwrite_a_selection_changed_in_flight() {
        let mut editor = editor("take-restaged");
        editor.transition_from = 2;
        editor.transition_to = 3;
        editor.transition_type = "dip_to_black".to_string();
        crate::app::set_local_storage(&transition_rollback_key("take-restaged"), "0|1|slide_left");

        editor.check_transition_status();

        assert_eq!(editor.transition_from, 2);
        assert_eq!(editor.transition_to, 3);
        assert_eq!(editor.transition_type, "dip_to_black");
    }

    /// A successful take writes no rollback, so the optimistic swap stands.
    #[test]
    fn absent_rollback_leaves_the_swap_in_place() {
        let mut editor = editor("take-succeeded");
        editor.transition_from = 1;
        editor.transition_to = 0;
        editor.transition_type = "slide_right".to_string();

        editor.check_transition_status();

        assert_eq!(editor.transition_from, 1);
        assert_eq!(editor.transition_to, 0);
        assert_eq!(editor.transition_type, "slide_right");
    }

    #[test]
    fn malformed_rollback_leaves_state_alone() {
        let mut editor = editor("take-malformed");
        editor.transition_from = 1;
        editor.transition_to = 0;
        crate::app::set_local_storage(&transition_rollback_key("take-malformed"), "not-a-rollback");

        editor.check_transition_status();

        assert_eq!(editor.transition_from, 1);
        assert_eq!(editor.transition_to, 0);
        assert!(
            crate::app::get_local_storage(&transition_rollback_key("take-malformed")).is_none()
        );
    }

    /// Non-directional types are their own inverse, so the guard must still match.
    #[test]
    fn rollback_matches_a_non_directional_type() {
        let mut editor = editor("take-fade");
        editor.transition_from = 1;
        editor.transition_to = 0;
        editor.transition_type = "fade".to_string();
        crate::app::set_local_storage(&transition_rollback_key("take-fade"), "0|1|fade");

        editor.check_transition_status();

        assert_eq!(editor.transition_from, 0);
        assert_eq!(editor.transition_to, 1);
        assert_eq!(editor.transition_type, "fade");
    }
}
