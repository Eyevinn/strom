use super::*;

impl MixerEditor {
    /// Reset the update throttle so the next update call goes through immediately.
    /// Call this before update methods when a discrete action (e.g. double-click reset)
    /// must not be dropped by the 50ms throttle.
    pub(super) fn bypass_throttle(&mut self) {
        self.last_update = instant::Instant::now() - std::time::Duration::from_millis(100);
    }

    /// Update a processing parameter (gate/comp).
    pub(super) fn update_processing_param(
        &mut self,
        ctx: &Context,
        index: usize,
        processor: &str,
        param: &str,
    ) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle continuous drag updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        let channel = &self.channels[index];

        let (element_suffix, gst_prop, value) = match (processor, param) {
            ("hpf", "enabled") => {
                let cutoff = if channel.hpf_enabled {
                    channel.hpf_freq
                } else {
                    0.0 // cutoff=0 enables GstBaseTransform passthrough
                };
                (
                    format!("hpf_{}", index),
                    "cutoff".to_string(),
                    PropertyValue::Float(cutoff as f64),
                )
            }
            ("hpf", "freq") => (
                format!("hpf_{}", index),
                "cutoff".to_string(),
                PropertyValue::Float(channel.hpf_freq as f64),
            ),
            ("gate", "threshold") => (
                format!("gate_{}", index),
                "gt".to_string(),
                PropertyValue::Float(db_to_linear_f64(channel.gate_threshold as f64)),
            ),
            ("gate", "attack") => (
                format!("gate_{}", index),
                "at".to_string(),
                PropertyValue::Float(channel.gate_attack as f64),
            ),
            ("gate", "release") => (
                format!("gate_{}", index),
                "rt".to_string(),
                PropertyValue::Float(channel.gate_release as f64),
            ),
            // Note: LSP gate does not have a settable range property
            // ("rr" doesn't exist, "gr" is read-only reduction meter)
            ("gate", "range") => return,
            ("comp", "threshold") => (
                format!("comp_{}", index),
                "al".to_string(),
                PropertyValue::Float(db_to_linear_f64(channel.comp_threshold as f64)),
            ),
            ("comp", "ratio") => (
                format!("comp_{}", index),
                "cr".to_string(),
                PropertyValue::Float(channel.comp_ratio as f64),
            ),
            ("comp", "attack") => (
                format!("comp_{}", index),
                "at".to_string(),
                PropertyValue::Float(channel.comp_attack as f64),
            ),
            ("comp", "release") => (
                format!("comp_{}", index),
                "rt".to_string(),
                PropertyValue::Float(channel.comp_release as f64),
            ),
            ("comp", "makeup") => (
                format!("comp_{}", index),
                "mk".to_string(),
                PropertyValue::Float(db_to_linear_f64(channel.comp_makeup as f64)),
            ),
            ("comp", "knee") => (
                format!("comp_{}", index),
                "kn".to_string(),
                PropertyValue::Float(
                    db_to_linear_f64(channel.comp_knee as f64).clamp(MIN_KNEE_LINEAR, 1.0),
                ),
            ),
            _ => return,
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:{}", self.block_id, element_suffix);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, &gst_prop, value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update an EQ band parameter.
    pub(super) fn update_eq_param(
        &mut self,
        ctx: &Context,
        index: usize,
        band: usize,
        param: &str,
    ) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle continuous drag updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        let channel = &self.channels[index];
        let (freq, gain, q) = channel.eq_bands[band];

        let (gst_prop, value) = match param {
            "freq" => (format!("f-{}", band), PropertyValue::Float(freq as f64)),
            "gain" => (
                format!("g-{}", band),
                PropertyValue::Float(db_to_linear_f64(gain as f64)),
            ),
            "q" => (format!("q-{}", band), PropertyValue::Float(q as f64)),
            _ => return,
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:eq_{}", self.block_id, index);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, &gst_prop, value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update a main bus processing parameter via API.
    pub(super) fn update_main_processing_param(
        &mut self,
        ctx: &Context,
        processor: &str,
        param: &str,
    ) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle continuous drag updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        let (element_suffix, gst_prop, value) = match (processor, param) {
            ("comp", "enabled") => (
                "main_comp".to_string(),
                "enabled".to_string(),
                PropertyValue::Bool(self.main_comp_enabled),
            ),
            ("comp", "threshold") => (
                "main_comp".to_string(),
                "al".to_string(),
                PropertyValue::Float(db_to_linear_f64(self.main_comp_threshold as f64)),
            ),
            ("comp", "ratio") => (
                "main_comp".to_string(),
                "cr".to_string(),
                PropertyValue::Float(self.main_comp_ratio as f64),
            ),
            ("comp", "attack") => (
                "main_comp".to_string(),
                "at".to_string(),
                PropertyValue::Float(self.main_comp_attack as f64),
            ),
            ("comp", "release") => (
                "main_comp".to_string(),
                "rt".to_string(),
                PropertyValue::Float(self.main_comp_release as f64),
            ),
            ("comp", "makeup") => (
                "main_comp".to_string(),
                "mk".to_string(),
                PropertyValue::Float(db_to_linear_f64(self.main_comp_makeup as f64)),
            ),
            ("comp", "knee") => (
                "main_comp".to_string(),
                "kn".to_string(),
                PropertyValue::Float(
                    db_to_linear_f64(self.main_comp_knee as f64).clamp(MIN_KNEE_LINEAR, 1.0),
                ),
            ),
            ("eq", "enabled") => (
                "main_eq".to_string(),
                "enabled".to_string(),
                PropertyValue::Bool(self.main_eq_enabled),
            ),
            ("limiter", "enabled") => (
                "main_limiter".to_string(),
                "enabled".to_string(),
                PropertyValue::Bool(self.main_limiter_enabled),
            ),
            ("limiter", "threshold") => (
                "main_limiter".to_string(),
                "th".to_string(),
                PropertyValue::Float(db_to_linear_f64(self.main_limiter_threshold as f64)),
            ),
            _ => return,
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:{}", self.block_id, element_suffix);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, &gst_prop, value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update a main bus EQ band parameter via API.
    pub(super) fn update_main_eq_param(&mut self, ctx: &Context, band: usize, param: &str) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle continuous drag updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        let (freq, gain, q) = self.main_eq_bands[band];

        let (gst_prop, value) = match param {
            "freq" => (format!("f-{}", band), PropertyValue::Float(freq as f64)),
            "gain" => (
                format!("g-{}", band),
                PropertyValue::Float(db_to_linear_f64(gain as f64)),
            ),
            "q" => (format!("q-{}", band), PropertyValue::Float(q as f64)),
            _ => return,
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:main_eq", self.block_id);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, &gst_prop, value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update a channel property via API.
    pub(super) fn update_channel_property(&mut self, ctx: &Context, index: usize, property: &str) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        let channel = &self.channels[index];

        // Map channel property to GStreamer element and property
        // The element_id format is "block_id:element_name"
        let (element_suffix, gst_prop, value) = match property {
            "gain" => (
                format!("gain_{}", index),
                "volume",
                PropertyValue::Float(db_to_linear_f64(channel.gain as f64)),
            ),
            "pan" => (
                format!("pan_{}", index),
                "panorama",
                PropertyValue::Float(channel.pan as f64),
            ),
            "fader" => {
                // If muted, set volume to 0, otherwise use fader value
                let effective_volume = if channel.mute {
                    0.0
                } else {
                    channel.fader as f64
                };
                (
                    format!("volume_{}", index),
                    "volume",
                    PropertyValue::Float(effective_volume),
                )
            }
            "mute" => {
                // Mute is implemented by setting volume to 0
                let effective_volume = if channel.mute {
                    0.0
                } else {
                    channel.fader as f64
                };
                (
                    format!("volume_{}", index),
                    "volume",
                    PropertyValue::Float(effective_volume),
                )
            }
            "gate_enabled" => (
                format!("gate_{}", index),
                "enabled",
                PropertyValue::Bool(channel.gate_enabled),
            ),
            "comp_enabled" => (
                format!("comp_{}", index),
                "enabled",
                PropertyValue::Bool(channel.comp_enabled),
            ),
            "eq_enabled" => (
                format!("eq_{}", index),
                "enabled",
                PropertyValue::Bool(channel.eq_enabled),
            ),
            "pfl" => (
                format!("pfl_volume_{}", index),
                "volume",
                PropertyValue::Float(if channel.pfl { 1.0 } else { 0.0 }),
            ),
            _ => return,
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:{}", self.block_id, element_suffix);
        let gst_prop = gst_prop.to_string();
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, &gst_prop, value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update main fader via API.
    pub(super) fn update_main_fader(&mut self, ctx: &Context) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        // Apply mute: if muted, send 0, otherwise send fader value
        let effective_volume = if self.main_mute {
            0.0
        } else {
            self.main_fader as f64
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:main_volume", self.block_id);
        let value = PropertyValue::Float(effective_volume);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, "volume", value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update main mute via API.
    pub(super) fn update_main_mute(&mut self, ctx: &Context) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Mute is implemented by setting volume to 0
        let effective_volume = if self.main_mute {
            0.0
        } else {
            self.main_fader as f64
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:main_volume", self.block_id);
        let value = PropertyValue::Float(effective_volume);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, "volume", value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update aux send level via API.
    pub(super) fn update_aux_send(&mut self, ctx: &Context, ch_idx: usize, aux_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle continuous drag updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        let level = self.channels[ch_idx].aux_sends[aux_idx] as f64;

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:aux_send_{}_{}", self.block_id, ch_idx, aux_idx);
        let value = PropertyValue::Float(level);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, "volume", value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update channel routing via API.
    /// Routing is implemented using volume elements - all destinations are always
    /// connected, we just set volume to 1.0 for active route and 0.0 for inactive.
    pub(super) fn update_routing(&mut self, ctx: &Context, ch_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        let to_main = self.channels[ch_idx].to_main;
        let to_grp = self.channels[ch_idx].to_grp;
        let num_groups = self.num_groups;

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let block_id = self.block_id.clone();
        let ctx = ctx.clone();

        // Update to_main volume
        let to_main_vol = if to_main { 1.0 } else { 0.0 };
        let to_main_id = format!("{}:to_main_vol_{}", block_id, ch_idx);

        let api_clone = api.clone();
        let ctx_clone = ctx.clone();
        crate::app::spawn_task(async move {
            if let Err(e) = api_clone
                .update_element_property(
                    &flow_id,
                    &to_main_id,
                    "volume",
                    PropertyValue::Float(to_main_vol),
                    ramp_ms,
                )
                .await
            {
                tracing::warn!("Mixer routing update failed: {}", e);
            }
            ctx_clone.request_repaint();
        });

        // Update each group route volume
        for (sg, &enabled) in to_grp.iter().enumerate().take(num_groups) {
            let route_sg_vol = if enabled { 1.0 } else { 0.0 };
            let to_grp_id = format!("{}:to_grp{}_vol_{}", block_id, sg, ch_idx);

            let api_clone = api.clone();
            let flow_id_clone = flow_id;
            let ctx_clone = ctx.clone();
            crate::app::spawn_task(async move {
                if let Err(e) = api_clone
                    .update_element_property(
                        &flow_id_clone,
                        &to_grp_id,
                        "volume",
                        PropertyValue::Float(route_sg_vol),
                        ramp_ms,
                    )
                    .await
                {
                    tracing::warn!("Mixer group routing update failed: {}", e);
                }
                ctx_clone.request_repaint();
            });
        }

        // Build routing description for logging
        let mut routes = Vec::new();
        if to_main {
            routes.push("Main".to_string());
        }
        for (sg, &enabled) in to_grp.iter().enumerate().take(num_groups) {
            if enabled {
                routes.push(format!("GRP{}", sg + 1));
            }
        }
        let routes_str = if routes.is_empty() {
            "None".to_string()
        } else {
            routes.join(", ")
        };
        tracing::info!("Routing updated: Ch {} -> {}", ch_idx + 1, routes_str);
    }

    /// Update group fader via API.
    pub(super) fn update_group_fader(&mut self, ctx: &Context, sg_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        let mute = self.groups[sg_idx].mute;
        let effective_volume = if mute {
            0.0
        } else {
            self.groups[sg_idx].fader as f64
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:group{}_volume", self.block_id, sg_idx);
        let value = PropertyValue::Float(effective_volume);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, "volume", value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update group mute via API.
    pub(super) fn update_group_mute(&mut self, ctx: &Context, sg_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        let mute = self.groups[sg_idx].mute;
        let effective_volume = if mute {
            0.0
        } else {
            self.groups[sg_idx].fader as f64
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:group{}_volume", self.block_id, sg_idx);
        let value = PropertyValue::Float(effective_volume);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, "volume", value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update aux master fader via API.
    pub(super) fn update_aux_master_fader(&mut self, ctx: &Context, aux_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        // Throttle updates
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        let mute = self.aux_masters[aux_idx].mute;
        let effective_volume = if mute {
            0.0
        } else {
            self.aux_masters[aux_idx].fader as f64
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:aux{}_volume", self.block_id, aux_idx);
        let value = PropertyValue::Float(effective_volume);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, "volume", value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }

    /// Update aux master mute via API.
    pub(super) fn update_aux_master_mute(&mut self, ctx: &Context, aux_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        let mute = self.aux_masters[aux_idx].mute;
        let effective_volume = if mute {
            0.0
        } else {
            self.aux_masters[aux_idx].fader as f64
        };

        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let element_id = format!("{}:aux{}_volume", self.block_id, aux_idx);
        let value = PropertyValue::Float(effective_volume);
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_element_property(&flow_id, &element_id, "volume", value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer API update failed: {}", e);
            }
            ctx.request_repaint();
        });
    }
}
