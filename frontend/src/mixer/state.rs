use super::*;

impl MixerEditor {
    /// Create a new mixer editor.
    pub fn new(flow_id: FlowId, block_id: String, num_channels: usize, api: ApiClient) -> Self {
        let channels = (1..=num_channels).map(ChannelStrip::new).collect();

        Self {
            flow_id,
            block_id,
            num_channels,
            num_aux_buses: 0,
            num_groups: 0,
            channels,
            groups: Vec::new(),
            aux_masters: Vec::new(),
            selection: None,
            active_control: ActiveControl::None,
            main_fader: DEFAULT_FADER,
            main_mute: false,
            solo_master_fader: DEFAULT_FADER,
            solo_mode: "pfl".to_string(),
            main_comp_enabled: false,
            main_comp_threshold: DEFAULT_COMP_THRESHOLD,
            main_comp_ratio: DEFAULT_COMP_RATIO,
            main_comp_attack: DEFAULT_COMP_ATTACK,
            main_comp_release: DEFAULT_COMP_RELEASE,
            main_comp_makeup: DEFAULT_COMP_MAKEUP,
            main_comp_knee: DEFAULT_COMP_KNEE,
            main_eq_enabled: false,
            main_eq_bands: DEFAULT_EQ_BANDS,
            main_limiter_enabled: false,
            main_limiter_threshold: DEFAULT_LIMITER_THRESHOLD,
            api,
            status: String::new(),
            error: None,
            live_updates: true,
            fade_ms: strom_types::mixer::DEFAULT_VOLUME_RAMP_MS,
            last_update: instant::Instant::now(),
            save_requested: false,
            is_reset: false,
            editing_label: None,
            strip_interacted: false,
            pipeline_running: false,
        }
    }

    /// Load channel values from block properties.
    pub fn load_from_properties(&mut self, properties: &HashMap<String, PropertyValue>) {
        // Load main fader and mute
        if let Some(PropertyValue::Float(f)) = properties.get("main_fader") {
            self.main_fader = *f as f32;
        }
        if let Some(PropertyValue::Bool(b)) = properties.get("main_mute") {
            self.main_mute = *b;
        }

        // Solo bus master + mode (PFL/AFL)
        if let Some(PropertyValue::Float(f)) = properties.get("pfl_level") {
            self.solo_master_fader = *f as f32;
        }
        if let Some(PropertyValue::String(s)) = properties.get("solo_mode") {
            self.solo_mode = s.clone();
        }

        // Load main bus processing
        if let Some(PropertyValue::Bool(b)) = properties.get("main_comp_enabled") {
            self.main_comp_enabled = *b;
        }
        if let Some(PropertyValue::Float(f)) = properties.get("main_comp_threshold") {
            self.main_comp_threshold = *f as f32;
        }
        if let Some(PropertyValue::Float(f)) = properties.get("main_comp_ratio") {
            self.main_comp_ratio = *f as f32;
        }
        if let Some(PropertyValue::Float(f)) = properties.get("main_comp_attack") {
            self.main_comp_attack = *f as f32;
        }
        if let Some(PropertyValue::Float(f)) = properties.get("main_comp_release") {
            self.main_comp_release = *f as f32;
        }
        if let Some(PropertyValue::Float(f)) = properties.get("main_comp_makeup") {
            self.main_comp_makeup = *f as f32;
        }
        if let Some(PropertyValue::Float(f)) = properties.get("main_comp_knee") {
            self.main_comp_knee = *f as f32;
        }
        if let Some(PropertyValue::Bool(b)) = properties.get("main_eq_enabled") {
            self.main_eq_enabled = *b;
        }
        for band in 0..4 {
            let band_num = band + 1;
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("main_eq{}_freq", band_num))
            {
                self.main_eq_bands[band].0 = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("main_eq{}_gain", band_num))
            {
                self.main_eq_bands[band].1 = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) = properties.get(&format!("main_eq{}_q", band_num))
            {
                self.main_eq_bands[band].2 = *f as f32;
            }
        }
        if let Some(PropertyValue::Bool(b)) = properties.get("main_limiter_enabled") {
            self.main_limiter_enabled = *b;
        }
        if let Some(PropertyValue::Float(f)) = properties.get("main_limiter_threshold") {
            self.main_limiter_threshold = *f as f32;
        }

        // Load number of aux buses
        self.num_aux_buses = properties
            .get("num_aux_buses")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as usize),
                PropertyValue::String(s) => s.parse().ok(),
                _ => None,
            })
            .unwrap_or(0)
            .min(MAX_AUX_BUSES);

        // Load number of groups
        self.num_groups = properties
            .get("num_groups")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as usize),
                PropertyValue::String(s) => s.parse().ok(),
                _ => None,
            })
            .unwrap_or(0)
            .min(MAX_GROUPS);

        // Initialize groups
        self.groups = (0..self.num_groups)
            .map(|i| {
                let mut sg = GroupStrip::new(i);
                if let Some(PropertyValue::Float(f)) =
                    properties.get(&format!("group{}_fader", i + 1))
                {
                    sg.fader = *f as f32;
                }
                if let Some(PropertyValue::Bool(b)) =
                    properties.get(&format!("group{}_mute", i + 1))
                {
                    sg.mute = *b;
                }
                sg
            })
            .collect();

        // Initialize aux masters
        self.aux_masters = (0..self.num_aux_buses)
            .map(|i| {
                let mut aux = AuxMaster::new(i);
                if let Some(PropertyValue::Float(f)) =
                    properties.get(&format!("aux{}_fader", i + 1))
                {
                    aux.fader = *f as f32;
                }
                if let Some(PropertyValue::Bool(b)) = properties.get(&format!("aux{}_mute", i + 1))
                {
                    aux.mute = *b;
                }
                aux
            })
            .collect();

        // Load per-channel properties
        for ch in &mut self.channels {
            let ch_num = ch.channel_num;

            // Label
            if let Some(PropertyValue::String(s)) = properties.get(&format!("ch{}_label", ch_num)) {
                ch.label = s.clone();
            }
            // Input gain
            if let Some(PropertyValue::Float(f)) = properties.get(&format!("ch{}_gain", ch_num)) {
                ch.gain = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) = properties.get(&format!("ch{}_pan", ch_num)) {
                ch.pan = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) = properties.get(&format!("ch{}_fader", ch_num)) {
                ch.fader = *f as f32;
            }
            if let Some(PropertyValue::Bool(b)) = properties.get(&format!("ch{}_mute", ch_num)) {
                ch.mute = *b;
            }
            if let Some(PropertyValue::Bool(b)) = properties.get(&format!("ch{}_pfl", ch_num)) {
                ch.pfl = *b;
            }
            // Routing to main
            if let Some(PropertyValue::Bool(b)) = properties.get(&format!("ch{}_to_main", ch_num)) {
                ch.to_main = *b;
            }
            // Routing to groups
            for sg in 0..MAX_GROUPS {
                if let Some(PropertyValue::Bool(b)) =
                    properties.get(&format!("ch{}_to_grp{}", ch_num, sg + 1))
                {
                    ch.to_grp[sg] = *b;
                }
            }
            // Aux send levels and pre/post
            for aux in 0..MAX_AUX_BUSES {
                if let Some(PropertyValue::Float(f)) =
                    properties.get(&format!("ch{}_aux{}_level", ch_num, aux + 1))
                {
                    ch.aux_sends[aux] = *f as f32;
                }
                if let Some(PropertyValue::Bool(b)) =
                    properties.get(&format!("ch{}_aux{}_pre", ch_num, aux + 1))
                {
                    ch.aux_pre[aux] = *b;
                }
            }
            // HPF
            if let Some(PropertyValue::Bool(b)) =
                properties.get(&format!("ch{}_hpf_enabled", ch_num))
            {
                ch.hpf_enabled = *b;
            }
            if let Some(PropertyValue::Float(f)) = properties.get(&format!("ch{}_hpf_freq", ch_num))
            {
                ch.hpf_freq = *f as f32;
            }
            // Gate
            if let Some(PropertyValue::Bool(b)) =
                properties.get(&format!("ch{}_gate_enabled", ch_num))
            {
                ch.gate_enabled = *b;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_gate_threshold", ch_num))
            {
                ch.gate_threshold = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_gate_attack", ch_num))
            {
                ch.gate_attack = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_gate_release", ch_num))
            {
                ch.gate_release = *f as f32;
            }
            // Compressor
            if let Some(PropertyValue::Bool(b)) =
                properties.get(&format!("ch{}_comp_enabled", ch_num))
            {
                ch.comp_enabled = *b;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_comp_threshold", ch_num))
            {
                ch.comp_threshold = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_comp_ratio", ch_num))
            {
                ch.comp_ratio = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_comp_attack", ch_num))
            {
                ch.comp_attack = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_comp_release", ch_num))
            {
                ch.comp_release = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_comp_makeup", ch_num))
            {
                ch.comp_makeup = *f as f32;
            }
            if let Some(PropertyValue::Float(f)) =
                properties.get(&format!("ch{}_comp_knee", ch_num))
            {
                ch.comp_knee = *f as f32;
            }
            // EQ
            if let Some(PropertyValue::Bool(b)) =
                properties.get(&format!("ch{}_eq_enabled", ch_num))
            {
                ch.eq_enabled = *b;
            }
            for band in 0..4 {
                let band_num = band + 1;
                if let Some(PropertyValue::Float(f)) =
                    properties.get(&format!("ch{}_eq{}_freq", ch_num, band_num))
                {
                    ch.eq_bands[band].0 = *f as f32;
                }
                if let Some(PropertyValue::Float(f)) =
                    properties.get(&format!("ch{}_eq{}_gain", ch_num, band_num))
                {
                    ch.eq_bands[band].1 = *f as f32;
                }
                if let Some(PropertyValue::Float(f)) =
                    properties.get(&format!("ch{}_eq{}_q", ch_num, band_num))
                {
                    ch.eq_bands[band].2 = *f as f32;
                }
            }
        }
    }

    /// Collect current mixer state as block properties, omitting values that
    /// match their defaults. Only non-default values and structural keys
    /// are persisted so storage stays minimal and backend defaults take over
    /// for anything not explicitly set.
    pub fn collect_properties(&self) -> HashMap<String, PropertyValue> {
        let mut props = HashMap::new();

        // Helper closures to insert only non-default values
        macro_rules! set_f {
            ($key:expr, $val:expr, $def:expr) => {
                if ($val - $def).abs() > f32::EPSILON {
                    props.insert($key, PropertyValue::Float($val as f64));
                }
            };
        }
        macro_rules! set_b {
            ($key:expr, $val:expr, $def:expr) => {
                if $val != $def {
                    props.insert($key, PropertyValue::Bool($val));
                }
            };
        }

        // Structural properties (always saved)
        props.insert(
            "num_channels".to_string(),
            PropertyValue::Int(self.num_channels as i64),
        );
        props.insert(
            "num_aux_buses".to_string(),
            PropertyValue::Int(self.num_aux_buses as i64),
        );
        props.insert(
            "num_groups".to_string(),
            PropertyValue::Int(self.num_groups as i64),
        );

        // Main bus
        set_f!("main_fader".to_string(), self.main_fader, DEFAULT_FADER);
        set_b!("main_mute".to_string(), self.main_mute, false);

        // Solo bus (master fader + mode)
        set_f!(
            "pfl_level".to_string(),
            self.solo_master_fader,
            DEFAULT_FADER
        );
        if self.solo_mode != "pfl" {
            props.insert(
                "solo_mode".to_string(),
                PropertyValue::String(self.solo_mode.clone()),
            );
        }
        set_b!(
            "main_comp_enabled".to_string(),
            self.main_comp_enabled,
            false
        );
        set_f!(
            "main_comp_threshold".to_string(),
            self.main_comp_threshold,
            DEFAULT_COMP_THRESHOLD
        );
        set_f!(
            "main_comp_ratio".to_string(),
            self.main_comp_ratio,
            DEFAULT_COMP_RATIO
        );
        set_f!(
            "main_comp_attack".to_string(),
            self.main_comp_attack,
            DEFAULT_COMP_ATTACK
        );
        set_f!(
            "main_comp_release".to_string(),
            self.main_comp_release,
            DEFAULT_COMP_RELEASE
        );
        set_f!(
            "main_comp_makeup".to_string(),
            self.main_comp_makeup,
            DEFAULT_COMP_MAKEUP
        );
        set_f!(
            "main_comp_knee".to_string(),
            self.main_comp_knee,
            DEFAULT_COMP_KNEE
        );
        set_b!("main_eq_enabled".to_string(), self.main_eq_enabled, false);
        for (band, (freq, gain, q)) in self.main_eq_bands.iter().enumerate() {
            let b = band + 1;
            let (df, dg, dq) = DEFAULT_EQ_BANDS[band];
            set_f!(format!("main_eq{}_freq", b), *freq, df);
            set_f!(format!("main_eq{}_gain", b), *gain, dg);
            set_f!(format!("main_eq{}_q", b), *q, dq);
        }
        set_b!(
            "main_limiter_enabled".to_string(),
            self.main_limiter_enabled,
            false
        );
        set_f!(
            "main_limiter_threshold".to_string(),
            self.main_limiter_threshold,
            DEFAULT_LIMITER_THRESHOLD
        );

        // Aux masters
        for aux in &self.aux_masters {
            let n = aux.index + 1;
            set_f!(format!("aux{}_fader", n), aux.fader, DEFAULT_FADER);
            set_b!(format!("aux{}_mute", n), aux.mute, false);
        }

        // Groups
        for sg in &self.groups {
            let n = sg.index + 1;
            set_f!(format!("group{}_fader", n), sg.fader, DEFAULT_FADER);
            set_b!(format!("group{}_mute", n), sg.mute, false);
        }

        // Per-channel
        for ch in &self.channels {
            let n = ch.channel_num;
            let default_label = format!("Ch {}", n);
            if ch.label != default_label {
                props.insert(
                    format!("ch{}_label", n),
                    PropertyValue::String(ch.label.clone()),
                );
            }
            set_f!(format!("ch{}_gain", n), ch.gain, DEFAULT_GAIN);
            set_f!(format!("ch{}_pan", n), ch.pan, DEFAULT_PAN);
            set_f!(format!("ch{}_fader", n), ch.fader, DEFAULT_FADER);
            set_b!(format!("ch{}_mute", n), ch.mute, false);
            set_b!(format!("ch{}_pfl", n), ch.pfl, false);
            set_b!(format!("ch{}_to_main", n), ch.to_main, true);
            for (sg, &enabled) in ch.to_grp.iter().enumerate().take(self.num_groups) {
                set_b!(format!("ch{}_to_grp{}", n, sg + 1), enabled, false);
            }
            for (aux, &default_pre) in DEFAULT_AUX_PRE.iter().enumerate().take(self.num_aux_buses) {
                set_f!(
                    format!("ch{}_aux{}_level", n, aux + 1),
                    ch.aux_sends[aux],
                    0.0
                );
                set_b!(
                    format!("ch{}_aux{}_pre", n, aux + 1),
                    ch.aux_pre[aux],
                    default_pre
                );
            }
            // HPF
            set_b!(format!("ch{}_hpf_enabled", n), ch.hpf_enabled, false);
            set_f!(format!("ch{}_hpf_freq", n), ch.hpf_freq, DEFAULT_HPF_FREQ);
            // Gate
            set_b!(format!("ch{}_gate_enabled", n), ch.gate_enabled, false);
            set_f!(
                format!("ch{}_gate_threshold", n),
                ch.gate_threshold,
                DEFAULT_GATE_THRESHOLD
            );
            set_f!(
                format!("ch{}_gate_attack", n),
                ch.gate_attack,
                DEFAULT_GATE_ATTACK
            );
            set_f!(
                format!("ch{}_gate_release", n),
                ch.gate_release,
                DEFAULT_GATE_RELEASE
            );
            // Compressor
            set_b!(format!("ch{}_comp_enabled", n), ch.comp_enabled, false);
            set_f!(
                format!("ch{}_comp_threshold", n),
                ch.comp_threshold,
                DEFAULT_COMP_THRESHOLD
            );
            set_f!(
                format!("ch{}_comp_ratio", n),
                ch.comp_ratio,
                DEFAULT_COMP_RATIO
            );
            set_f!(
                format!("ch{}_comp_attack", n),
                ch.comp_attack,
                DEFAULT_COMP_ATTACK
            );
            set_f!(
                format!("ch{}_comp_release", n),
                ch.comp_release,
                DEFAULT_COMP_RELEASE
            );
            set_f!(
                format!("ch{}_comp_makeup", n),
                ch.comp_makeup,
                DEFAULT_COMP_MAKEUP
            );
            set_f!(
                format!("ch{}_comp_knee", n),
                ch.comp_knee,
                DEFAULT_COMP_KNEE
            );
            // EQ
            set_b!(format!("ch{}_eq_enabled", n), ch.eq_enabled, false);
            for (band, (freq, gain, q)) in ch.eq_bands.iter().enumerate() {
                let b = band + 1;
                let (df, dg, dq) = DEFAULT_EQ_BANDS[band];
                set_f!(format!("ch{}_eq{}_freq", n, b), *freq, df);
                set_f!(format!("ch{}_eq{}_gain", n, b), *gain, dg);
                set_f!(format!("ch{}_eq{}_q", n, b), *q, dq);
            }
        }

        props
    }

    /// Reset all mixer parameters to defaults.
    /// Resets in-memory state and sets `reset_properties` so the save
    /// only writes structural keys, letting backend defaults take over.
    pub(super) fn reset_to_defaults(&mut self) {
        // Main bus
        self.main_fader = DEFAULT_FADER;
        self.main_mute = false;
        self.solo_master_fader = DEFAULT_FADER;
        // solo_mode is intentionally NOT reset — it's a structural choice
        // (which bus the solo path taps from), not a value to flatten.
        self.main_comp_enabled = false;
        self.main_comp_threshold = DEFAULT_COMP_THRESHOLD;
        self.main_comp_ratio = DEFAULT_COMP_RATIO;
        self.main_comp_attack = DEFAULT_COMP_ATTACK;
        self.main_comp_release = DEFAULT_COMP_RELEASE;
        self.main_comp_makeup = DEFAULT_COMP_MAKEUP;
        self.main_comp_knee = DEFAULT_COMP_KNEE;
        self.main_eq_enabled = false;
        self.main_eq_bands = DEFAULT_EQ_BANDS;
        self.main_limiter_enabled = false;
        self.main_limiter_threshold = DEFAULT_LIMITER_THRESHOLD;

        // Aux masters
        for aux in &mut self.aux_masters {
            aux.fader = DEFAULT_FADER;
            aux.mute = false;
        }

        // Groups
        for sg in &mut self.groups {
            sg.fader = DEFAULT_FADER;
            sg.mute = false;
        }

        // Channels
        for ch in &mut self.channels {
            ch.gain = DEFAULT_GAIN;
            ch.pan = DEFAULT_PAN;
            ch.fader = DEFAULT_FADER;
            ch.mute = false;
            ch.pfl = false;
            ch.to_main = true;
            ch.to_grp = [false; MAX_GROUPS];
            ch.aux_sends = [0.0; MAX_AUX_BUSES];
            ch.aux_pre = DEFAULT_AUX_PRE;
            ch.hpf_enabled = false;
            ch.hpf_freq = DEFAULT_HPF_FREQ;
            ch.gate_enabled = false;
            ch.gate_threshold = DEFAULT_GATE_THRESHOLD;
            ch.gate_attack = DEFAULT_GATE_ATTACK;
            ch.gate_release = DEFAULT_GATE_RELEASE;
            ch.comp_enabled = false;
            ch.comp_threshold = DEFAULT_COMP_THRESHOLD;
            ch.comp_ratio = DEFAULT_COMP_RATIO;
            ch.comp_attack = DEFAULT_COMP_ATTACK;
            ch.comp_release = DEFAULT_COMP_RELEASE;
            ch.comp_makeup = DEFAULT_COMP_MAKEUP;
            ch.comp_knee = DEFAULT_COMP_KNEE;
            ch.eq_enabled = false;
            ch.eq_bands = DEFAULT_EQ_BANDS;
        }

        self.selection = None;
        self.is_reset = true;
    }

    /// Collect only structural properties (after reset).
    /// Backend uses its own matching defaults for everything else.
    pub fn collect_structural_properties(&self) -> HashMap<String, PropertyValue> {
        let mut props = HashMap::new();
        props.insert(
            "num_channels".to_string(),
            PropertyValue::Int(self.num_channels as i64),
        );
        props.insert(
            "num_aux_buses".to_string(),
            PropertyValue::Int(self.num_aux_buses as i64),
        );
        props.insert(
            "num_groups".to_string(),
            PropertyValue::Int(self.num_groups as i64),
        );
        props
    }
}
