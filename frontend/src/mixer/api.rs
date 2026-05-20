use super::*;

impl MixerEditor {
    /// Reset the update throttle so the next update call goes through immediately.
    /// Call this before update methods when a discrete action (e.g. double-click reset)
    /// must not be dropped by the 50ms throttle.
    pub(super) fn bypass_throttle(&mut self) {
        self.last_update = instant::Instant::now() - std::time::Duration::from_millis(100);
    }

    /// Send a single block-level exposed property to the backend block-properties
    /// endpoint. The backend resolves the underlying GStreamer element via the
    /// block's PropertyMapping and applies any declared transform
    /// (`bool_to_volume`, `db_to_linear`, …) — frontend stays unaware of internal
    /// element names like `pfl_volume_0`, `gate_0`, `to_grp_*_vol_*`, etc.
    fn spawn_block_prop_update(&self, ctx: &Context, prop_name: String, value: PropertyValue) {
        let ramp_ms = Some(self.fade_ms);
        let api = self.api.clone();
        let flow_id = self.flow_id;
        let block_id = self.block_id.clone();
        let ctx = ctx.clone();

        crate::app::spawn_task(async move {
            if let Err(e) = api
                .update_block_property(&flow_id, &block_id, &prop_name, value, ramp_ms)
                .await
            {
                tracing::warn!("Mixer block-property update failed ({}): {}", prop_name, e);
            }
            ctx.request_repaint();
        });
    }

    /// Update a processing parameter (HPF/gate/comp).
    ///
    /// Values are sent in user-facing units (Hz, dB, ms) — the backend's
    /// `db_to_linear` transform handles the conversion to the underlying
    /// LV2/LSP element. HPF enabled remains on the element path because the
    /// "off" state is the cutoff=0 passthrough trick, not a real bool.
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
        let ch1 = index + 1;

        // HPF enabled is implemented via cutoff=0 passthrough on the element,
        // not a real bool property — stays on the element path.
        if processor == "hpf" && param == "enabled" {
            let cutoff = if channel.hpf_enabled {
                channel.hpf_freq
            } else {
                0.0
            };
            let ramp_ms = Some(self.fade_ms);
            let api = self.api.clone();
            let flow_id = self.flow_id;
            let element_id = format!("{}:hpf_{}", self.block_id, index);
            let ctx = ctx.clone();
            crate::app::spawn_task(async move {
                if let Err(e) = api
                    .update_element_property(
                        &flow_id,
                        &element_id,
                        "cutoff",
                        PropertyValue::Float(cutoff as f64),
                        ramp_ms,
                    )
                    .await
                {
                    tracing::warn!("Mixer HPF passthrough update failed: {}", e);
                }
                ctx.request_repaint();
            });
            return;
        }

        let (prop_name, value) = match (processor, param) {
            ("hpf", "freq") => (
                format!("ch{}_hpf_freq", ch1),
                PropertyValue::Float(channel.hpf_freq as f64),
            ),
            ("gate", "threshold") => (
                format!("ch{}_gate_threshold", ch1),
                PropertyValue::Float(channel.gate_threshold as f64),
            ),
            ("gate", "attack") => (
                format!("ch{}_gate_attack", ch1),
                PropertyValue::Float(channel.gate_attack as f64),
            ),
            ("gate", "release") => (
                format!("ch{}_gate_release", ch1),
                PropertyValue::Float(channel.gate_release as f64),
            ),
            // LSP gate has no settable range property
            ("gate", "range") => return,
            ("comp", "threshold") => (
                format!("ch{}_comp_threshold", ch1),
                PropertyValue::Float(channel.comp_threshold as f64),
            ),
            ("comp", "ratio") => (
                format!("ch{}_comp_ratio", ch1),
                PropertyValue::Float(channel.comp_ratio as f64),
            ),
            ("comp", "attack") => (
                format!("ch{}_comp_attack", ch1),
                PropertyValue::Float(channel.comp_attack as f64),
            ),
            ("comp", "release") => (
                format!("ch{}_comp_release", ch1),
                PropertyValue::Float(channel.comp_release as f64),
            ),
            ("comp", "makeup") => (
                format!("ch{}_comp_makeup", ch1),
                PropertyValue::Float(channel.comp_makeup as f64),
            ),
            ("comp", "knee") => (
                format!("ch{}_comp_knee", ch1),
                PropertyValue::Float(channel.comp_knee as f64),
            ),
            _ => return,
        };

        self.spawn_block_prop_update(ctx, prop_name, value);
    }

    /// Update an EQ band parameter via the block-properties endpoint.
    /// Gain is in dB (backend applies `db_to_linear`).
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
        let ch1 = index + 1;
        let band1 = band + 1;

        let (prop_name, value) = match param {
            "freq" => (
                format!("ch{}_eq{}_freq", ch1, band1),
                PropertyValue::Float(freq as f64),
            ),
            "gain" => (
                format!("ch{}_eq{}_gain", ch1, band1),
                PropertyValue::Float(gain as f64),
            ),
            "q" => (
                format!("ch{}_eq{}_q", ch1, band1),
                PropertyValue::Float(q as f64),
            ),
            _ => return,
        };

        self.spawn_block_prop_update(ctx, prop_name, value);
    }

    /// Update a main bus processing parameter via the block-properties endpoint.
    /// dB-scale values are sent as dB; the backend handles `db_to_linear`.
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

        let (prop_name, value) = match (processor, param) {
            ("comp", "enabled") => (
                "main_comp_enabled".to_string(),
                PropertyValue::Bool(self.main_comp_enabled),
            ),
            ("comp", "threshold") => (
                "main_comp_threshold".to_string(),
                PropertyValue::Float(self.main_comp_threshold as f64),
            ),
            ("comp", "ratio") => (
                "main_comp_ratio".to_string(),
                PropertyValue::Float(self.main_comp_ratio as f64),
            ),
            ("comp", "attack") => (
                "main_comp_attack".to_string(),
                PropertyValue::Float(self.main_comp_attack as f64),
            ),
            ("comp", "release") => (
                "main_comp_release".to_string(),
                PropertyValue::Float(self.main_comp_release as f64),
            ),
            ("comp", "makeup") => (
                "main_comp_makeup".to_string(),
                PropertyValue::Float(self.main_comp_makeup as f64),
            ),
            ("comp", "knee") => (
                "main_comp_knee".to_string(),
                PropertyValue::Float(self.main_comp_knee as f64),
            ),
            ("eq", "enabled") => (
                "main_eq_enabled".to_string(),
                PropertyValue::Bool(self.main_eq_enabled),
            ),
            ("limiter", "enabled") => (
                "main_limiter_enabled".to_string(),
                PropertyValue::Bool(self.main_limiter_enabled),
            ),
            ("limiter", "threshold") => (
                "main_limiter_threshold".to_string(),
                PropertyValue::Float(self.main_limiter_threshold as f64),
            ),
            _ => return,
        };

        self.spawn_block_prop_update(ctx, prop_name, value);
    }

    /// Update a main bus EQ band parameter via the block-properties endpoint.
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
        let band1 = band + 1;

        let (prop_name, value) = match param {
            "freq" => (
                format!("main_eq{}_freq", band1),
                PropertyValue::Float(freq as f64),
            ),
            "gain" => (
                format!("main_eq{}_gain", band1),
                PropertyValue::Float(gain as f64),
            ),
            "q" => (
                format!("main_eq{}_q", band1),
                PropertyValue::Float(q as f64),
            ),
            _ => return,
        };

        self.spawn_block_prop_update(ctx, prop_name, value);
    }

    /// Update a channel property via API.
    ///
    /// Most properties go through the block-properties endpoint with block-level
    /// units (Bool, dB, etc). `fader`/`mute` continue to use the element path
    /// because they share the volume element with mute-as-volume=0 semantics —
    /// migrating cleanly requires switching mute to the GstVolume `mute`
    /// property in the builder, which is a separate refactor.
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
        let ch1 = index + 1;

        // All channel properties now flow through the block-properties endpoint
        // — fader (Float) and mute (Bool) write to separate GstVolume properties
        // (volume / mute) so they no longer share a single value with the
        // "volume=0 if muted" trick.
        let (prop_name, value) = match property {
            "pfl" => (format!("ch{}_pfl", ch1), PropertyValue::Bool(channel.pfl)),
            "afl" => (format!("ch{}_afl", ch1), PropertyValue::Bool(channel.afl)),
            "gain" => (
                format!("ch{}_gain", ch1),
                PropertyValue::Float(channel.gain as f64),
            ),
            "pan" => (
                format!("ch{}_pan", ch1),
                PropertyValue::Float(channel.pan as f64),
            ),
            "fader" => (
                format!("ch{}_fader", ch1),
                PropertyValue::Float(channel.fader as f64),
            ),
            "mute" => (format!("ch{}_mute", ch1), PropertyValue::Bool(channel.mute)),
            "gate_enabled" => (
                format!("ch{}_gate_enabled", ch1),
                PropertyValue::Bool(channel.gate_enabled),
            ),
            "comp_enabled" => (
                format!("ch{}_comp_enabled", ch1),
                PropertyValue::Bool(channel.comp_enabled),
            ),
            "eq_enabled" => (
                format!("ch{}_eq_enabled", ch1),
                PropertyValue::Bool(channel.eq_enabled),
            ),
            _ => return,
        };

        self.spawn_block_prop_update(ctx, prop_name, value);
    }

    /// Update main fader via the block-properties endpoint.
    pub(super) fn update_main_fader(&mut self, ctx: &Context) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        self.spawn_block_prop_update(
            ctx,
            "main_fader".to_string(),
            PropertyValue::Float(self.main_fader as f64),
        );
    }

    /// Update main mute via the block-properties endpoint (Bool to GstVolume.mute).
    pub(super) fn update_main_mute(&mut self, ctx: &Context) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }
        self.spawn_block_prop_update(
            ctx,
            "main_mute".to_string(),
            PropertyValue::Bool(self.main_mute),
        );
    }

    /// Push the monitor bus master fader through the block-properties endpoint.
    /// Monitor has no mute, so no element-path special case is needed.
    pub(super) fn update_monitor_master_fader(&mut self, ctx: &Context) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        self.spawn_block_prop_update(
            ctx,
            "monitor_fader".to_string(),
            PropertyValue::Float(self.monitor_fader as f64),
        );
    }

    /// Update aux send level via the block-properties endpoint.
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
        self.spawn_block_prop_update(
            ctx,
            format!("ch{}_aux{}_level", ch_idx + 1, aux_idx + 1),
            PropertyValue::Float(level),
        );
    }

    /// Push channel routing toggles (`to_main`, `to_grp{X}`) through the
    /// block-properties endpoint as Bools. The backend's `bool_to_volume`
    /// transform handles the underlying volume-element 0/1 write.
    pub(super) fn update_routing(&mut self, ctx: &Context, ch_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }

        let ch1 = ch_idx + 1;
        let to_main = self.channels[ch_idx].to_main;
        let to_grp = self.channels[ch_idx].to_grp;
        let num_groups = self.num_groups;

        self.spawn_block_prop_update(
            ctx,
            format!("ch{}_to_main", ch1),
            PropertyValue::Bool(to_main),
        );

        for (sg, &enabled) in to_grp.iter().enumerate().take(num_groups) {
            self.spawn_block_prop_update(
                ctx,
                format!("ch{}_to_grp{}", ch1, sg + 1),
                PropertyValue::Bool(enabled),
            );
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
        tracing::info!("Routing updated: Ch {} -> {}", ch1, routes_str);
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

        self.spawn_block_prop_update(
            ctx,
            format!("group{}_fader", sg_idx + 1),
            PropertyValue::Float(self.groups[sg_idx].fader as f64),
        );
    }

    /// Update group mute via the block-properties endpoint (Bool to GstVolume.mute).
    pub(super) fn update_group_mute(&mut self, ctx: &Context, sg_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }
        self.spawn_block_prop_update(
            ctx,
            format!("group{}_mute", sg_idx + 1),
            PropertyValue::Bool(self.groups[sg_idx].mute),
        );
    }

    /// Update aux master fader via the block-properties endpoint.
    pub(super) fn update_aux_master_fader(&mut self, ctx: &Context, aux_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }
        if self.last_update.elapsed().as_millis() < 50 {
            return;
        }
        self.last_update = instant::Instant::now();

        self.spawn_block_prop_update(
            ctx,
            format!("aux{}_fader", aux_idx + 1),
            PropertyValue::Float(self.aux_masters[aux_idx].fader as f64),
        );
    }

    /// Update aux master mute via the block-properties endpoint (Bool to GstVolume.mute).
    pub(super) fn update_aux_master_mute(&mut self, ctx: &Context, aux_idx: usize) {
        if !self.live_updates || !self.pipeline_running {
            return;
        }
        self.spawn_block_prop_update(
            ctx,
            format!("aux{}_mute", aux_idx + 1),
            PropertyValue::Bool(self.aux_masters[aux_idx].mute),
        );
    }
}
