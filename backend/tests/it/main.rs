//! The integration tests, in one binary. Each test file is a module here.
//!
//! One binary links `strom` once, where one binary per file linked it once per
//! file, and its tests run in parallel with each other instead of one binary at
//! a time. A new test file goes here, as `pub mod <name>;` below.
//!
//! A test that changes process-global state (registers a stand-in element
//! under a real name, removes or re-ranks a factory, installs a log handler
//! that counts every critical) cannot share a process with the rest. It gets
//! its own `[[test]]` target in `backend/Cargo.toml`, which says why.

#[path = "../common/mod.rs"]
pub mod common;
#[path = "../vision_mixer_input_format/mod.rs"]
pub mod vision_mixer_input_format;
// Helpers several test files share. A file loaded as a module more than once
// (one `#[path]` per test file) is a clippy error in one crate.
#[path = "../common/http_file.rs"]
pub mod http_file;
#[path = "../common/recorder.rs"]
pub mod recorder;
#[path = "../common/stinger.rs"]
pub mod stinger;

pub mod aes67_buffersplit_queue_test;
pub mod api_not_found_test;
pub mod audioprocessor_test;
pub mod bare_link_pads_test;
pub mod block_properties_persist_test;
pub mod block_property_range_test;
pub mod build_version_test;
pub mod compositor_cpu_input_memory_test;
pub mod cors_expose_headers_test;
pub mod decklink_output_audio_test;
pub mod decklink_output_input_test;
pub mod drop_on_latency_test;
pub mod efpsrt_data_roundtrip_test;
pub mod enum_property_read_test;
pub mod failed_start_teardown_test;
pub mod flow_id_honoured_test;
pub mod gl_memory_link_test;
pub mod gl_probe_test;
pub mod gpu_selection_test;
pub mod html_remote_control_property_test;
pub mod inter_gl_context_test;
pub mod internal_pipeline_thread_priority_test;
pub mod latency_diagnosis_test;
pub mod latency_redistribute_test;
pub mod live_recorder_test;
pub mod liveaudiorouter_test;
pub mod local_input_audio_latency_test;
pub mod local_input_memory_test;
pub mod local_input_video_latency_test;
pub mod local_output_test;
pub mod macos_app_nap_test;
pub mod mcp_endpoint_test;
pub mod media_download_test;
pub mod media_player_control_test;
pub mod media_player_error_event_test;
pub mod media_player_hls_test;
pub mod media_upload_test;
pub mod mediaplayer_gl_context_test;
pub mod mpegtssrt_absent_caller_test;
pub mod mpegtssrt_input_audio_pid_order_test;
pub mod mpegtssrt_input_dual_audio_test;
pub mod mpegtssrt_input_program_change_test;
pub mod mpegtssrt_streamheader_test;
pub mod openapi_test;
pub mod output_refusal_test;
pub mod pipeline_lifecycle_test;
pub mod pipeline_start_failure_test;
pub mod pipeline_thread_reset_test;
pub mod port_pool_test;
pub mod property_not_found_status_test;
pub mod property_type_panic_test;
pub mod recorder_stall_test;
pub mod recorder_test;
pub mod rtmp_output_test;
pub mod rtp_hdrext_aggregation_test;
pub mod shader_validation_test;
pub mod srt_output_peerless_caller_test;
pub mod srt_relay_test;
pub mod stinger_api_test;
pub mod stinger_bench;
pub mod stinger_clip_test;
pub mod stinger_prores_test;
pub mod stinger_test;
pub mod tams_output_test;
pub mod test_layout_test;
pub mod thread_port_lifetime_test;
pub mod thumbnail_tap_gl_test;
pub mod video_input_bridge_gl_test;
pub mod videoenc_profile_negotiation_test;
pub mod videoformat_upstream_caps_test;
pub mod vision_mixer_cpu_input_memory_test;
pub mod vision_mixer_cpu_test;
pub mod vision_mixer_fx_passthrough_test;
pub mod vision_mixer_fx_test;
pub mod vision_mixer_gl_allocation_test;
pub mod vision_mixer_gl_output_test;
pub mod vision_mixer_overlay_upload_test;
pub mod vision_mixer_signals_test;
pub mod vision_mixer_source_resize_test;
pub mod vision_mixer_underlay_upload_test;
pub mod vision_mixer_videoformat_input_test;
pub mod volume_ramp_test;
pub mod whep_input_video_test;
pub mod whep_output_latency_test;
pub mod whep_output_level_change_test;
pub mod whep_output_profile_filter_test;
pub mod whip_idle_slot_test;
pub mod whip_output_latency_test;
pub mod whip_output_video_test;
pub mod whip_session_branch_test;
pub mod whip_slot_caps_reuse_test;
pub mod whip_slot_decoder_reuse_test;
pub mod whip_slot_native_video_test;
pub mod whip_video_damage_test;
pub mod whip_whep_ice_transport_policy_test;
