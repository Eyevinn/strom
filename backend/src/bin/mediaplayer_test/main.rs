//! Media Player Interactive Test Harness
//!
//! A test harness that runs the Strom server, creates MediaPlayer → MPEGTSSRT flows,
//! and guides a human through testing media player functionality.

mod api;
mod flow_builder;
mod prompt;
mod server;

use std::env;
use std::time::Duration;

use prompt::{print_banner, print_header, print_info, print_success, TestResult};

/// Configuration for the test harness
struct Config {
    srt_uri: String,
    media_dir: String,
    server_port: u16,
    test_file: String,
    start_phase: Option<u32>,
}

impl Config {
    fn from_env() -> Self {
        // Check for --phase or -p argument
        let args: Vec<String> = env::args().collect();
        let start_phase = args
            .iter()
            .position(|a| a == "--phase" || a == "-p")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok());

        Self {
            srt_uri: env::var("TEST_SRT_URI")
                .unwrap_or_else(|_| "srt://:5100?mode=listener".to_string()),
            media_dir: env::var("TEST_MEDIA_DIR").unwrap_or_else(|_| "./media".to_string()),
            server_port: env::var("STROM_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8080),
            test_file: "BigBuckBunny.mp4".to_string(),
            start_phase,
        }
    }
}

/// A single test case
struct TestCase {
    id: &'static str,
    name: &'static str,
    decode_mode: bool,
    action: TestAction,
    expected: &'static [&'static str],
}

#[derive(Clone)]
#[allow(dead_code)] // Some variants reserved for future tests
enum TestAction {
    StartFlow,
    StartFlowWithPlaylist(Vec<&'static str>), // Start with multiple files (no loop)
    StartFlowWithPlaylistLoop(Vec<&'static str>), // Start with multiple files (loop enabled)
    StartDecodeMode(Vec<&'static str>), // Decode mode: MediaPlayer -> VideoEncoder -> MPEGTSSRT
    StartDecodeModeLoop(Vec<&'static str>), // Decode mode with loop
    Pause,
    Play,
    SeekForward(u64), // nanoseconds
    SeekTo(u64),      // nanoseconds
    Next,             // Go to next file in playlist
    Previous,         // Go to previous file in playlist
    GotoFile(usize),  // Go to specific file index
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env();

    print_banner();

    // Show phase info if starting from specific phase
    if let Some(phase) = config.start_phase {
        print_info(&format!(
            "Starting from phase {} (use --phase N or -p N to change)",
            phase
        ));
    } else {
        print_info("Running all phases (use --phase N or -p N to start from specific phase)");
    }

    // Start the server
    print_info("Starting Strom server (with GUI)...");
    let mut server = server::ServerManager::start(config.server_port).await?;

    // Wait for server to be ready
    print_info("Waiting for server to be ready...");
    let api = api::StromClient::new(config.server_port);
    api.wait_for_ready(Duration::from_secs(30)).await?;
    print_success("Server ready!");

    // Clean up any old test flows (using __MPTEST__ prefix to avoid conflicts)
    print_info("Cleaning up old test flows...");
    api.delete_flows_by_prefix("__MPTEST__").await?;

    // Show instructions
    println!();
    print_info(&format!(
        "Open your SRT player:\n       ffplay srt://127.0.0.1:{}",
        config
            .srt_uri
            .split(':')
            .next_back()
            .unwrap_or("5100")
            .split('?')
            .next()
            .unwrap_or("5100")
    ));
    println!();
    prompt::wait_for_enter("Press Enter when your player is ready...");

    // Define test cases
    // NOTE: Decode mode is skipped because MPEGTSSRT only accepts encoded video (H.264/H.265),
    // not raw video. We'd need a video encoder block for decode mode tests.
    let tests = vec![
        TestCase {
            id: "1.1",
            name: "Basic video+audio playback (passthrough mode)",
            decode_mode: false,
            action: TestAction::StartFlow,
            expected: &[
                "Video should be playing smoothly",
                "Audio should be in sync with video",
                "No artifacts or glitches",
            ],
        },
        TestCase {
            id: "1.2",
            name: "Pause",
            decode_mode: false,
            action: TestAction::Pause,
            expected: &["Video should be frozen", "Audio should have stopped"],
        },
        TestCase {
            id: "1.3",
            name: "Resume",
            decode_mode: false,
            action: TestAction::Play,
            expected: &["Video should resume playing", "Audio should resume"],
        },
        // NOTE: Seek tests skipped - seeking not working properly with live sinks (sync=true)
        // TODO: Fix seek functionality and re-enable these tests
        // TestCase { id: "1.4", name: "Seek to 5 minutes", ... },
        // TestCase { id: "1.5", name: "Seek to start", ... },

        // --- Playlist Navigation Tests ---
        // Using longer files: BigBuckBunny (~10min), ElephantsDream (~11min), Sintel (~15min)
        TestCase {
            id: "2.1",
            name: "Playlist: Start with multiple files",
            decode_mode: false,
            action: TestAction::StartFlowWithPlaylist(vec![
                "BigBuckBunny.mp4",
                "ElephantsDream.mp4",
                "Sintel.mp4",
            ]),
            expected: &[
                "First file (BigBuckBunny) should be playing",
                "Video and audio should be in sync",
            ],
        },
        TestCase {
            id: "2.2",
            name: "Playlist: Next file",
            decode_mode: false,
            action: TestAction::Next,
            expected: &[
                "Should switch to second file (ElephantsDream)",
                "Transition should be smooth",
            ],
        },
        TestCase {
            id: "2.3",
            name: "Playlist: Next file again",
            decode_mode: false,
            action: TestAction::Next,
            expected: &[
                "Should switch to third file (Sintel)",
                "Transition should be smooth",
            ],
        },
        TestCase {
            id: "2.4",
            name: "Playlist: Previous file",
            decode_mode: false,
            action: TestAction::Previous,
            expected: &[
                "Should go back to second file (ElephantsDream)",
                "Transition should be smooth",
            ],
        },
        TestCase {
            id: "2.5",
            name: "Playlist: Go to first file",
            decode_mode: false,
            action: TestAction::GotoFile(0),
            expected: &[
                "Should jump to first file (BigBuckBunny)",
                "Transition should be smooth",
            ],
        },
        // --- Decode Mode Tests (MediaPlayer -> VideoEncoder -> MPEGTSSRT) ---
        TestCase {
            id: "3.1",
            name: "Decode Mode: Basic playback with re-encoding",
            decode_mode: true,
            action: TestAction::StartDecodeMode(vec!["BigBuckBunny.mp4"]),
            expected: &[
                "Video should be playing (re-encoded via VideoEncoder)",
                "Audio should be in sync",
                "Quality may differ from passthrough (re-encoded)",
            ],
        },
        TestCase {
            id: "3.2",
            name: "Decode Mode: Pause",
            decode_mode: true,
            action: TestAction::Pause,
            expected: &["Video should freeze", "Audio should stop"],
        },
        TestCase {
            id: "3.3",
            name: "Decode Mode: Resume",
            decode_mode: true,
            action: TestAction::Play,
            expected: &["Video should resume", "Audio should resume"],
        },
        TestCase {
            id: "3.4",
            name: "Decode Mode: Playlist with multiple files",
            decode_mode: true,
            action: TestAction::StartDecodeMode(vec!["BigBuckBunny.mp4", "ElephantsDream.mp4"]),
            expected: &[
                "First file (BigBuckBunny) should be playing",
                "Re-encoded video quality",
            ],
        },
        TestCase {
            id: "3.5",
            name: "Decode Mode: Next file",
            decode_mode: true,
            action: TestAction::Next,
            expected: &[
                "Should switch to ElephantsDream",
                "Transition may have brief glitch (re-encoding restart)",
            ],
        },
        TestCase {
            id: "3.6",
            name: "Decode Mode: Previous file",
            decode_mode: true,
            action: TestAction::Previous,
            expected: &["Should go back to BigBuckBunny"],
        },
        // --- Edge Case Tests ---
        TestCase {
            id: "4.1",
            name: "Edge: Previous at start of playlist",
            decode_mode: false,
            action: TestAction::StartFlowWithPlaylist(vec![
                "BigBuckBunny.mp4",
                "ElephantsDream.mp4",
                "Sintel.mp4",
            ]),
            expected: &["First file (BigBuckBunny) should be playing"],
        },
        TestCase {
            id: "4.2",
            name: "Edge: Previous at start (should reject)",
            decode_mode: false,
            action: TestAction::Previous,
            expected: &[
                "Command should be rejected (Already at first file)",
                "Playback should continue on first file (BigBuckBunny)",
            ],
        },
        TestCase {
            id: "4.3",
            name: "Edge: Go to last file",
            decode_mode: false,
            action: TestAction::GotoFile(2),
            expected: &["Should jump to last file (Sintel)"],
        },
        TestCase {
            id: "4.4",
            name: "Edge: Next at end of playlist (no loop)",
            decode_mode: false,
            action: TestAction::Next,
            expected: &[
                "Command should be rejected (Already at last file)",
                "Playback should continue on last file (Sintel)",
            ],
        },
        // --- Loop Playlist Tests ---
        TestCase {
            id: "5.1",
            name: "Loop: Start playlist with loop enabled",
            decode_mode: false,
            action: TestAction::StartFlowWithPlaylistLoop(vec![
                "BigBuckBunny.mp4",
                "ElephantsDream.mp4",
                "Sintel.mp4",
            ]),
            expected: &["First file (BigBuckBunny) should be playing"],
        },
        TestCase {
            id: "5.2",
            name: "Loop: Go to last file",
            decode_mode: false,
            action: TestAction::GotoFile(2),
            expected: &["Should be on last file (Sintel)"],
        },
        TestCase {
            id: "5.3",
            name: "Loop: Next should wrap to first",
            decode_mode: false,
            action: TestAction::Next,
            expected: &[
                "Should wrap to first file (BigBuckBunny)",
                "Playback should continue seamlessly",
            ],
        },
        TestCase {
            id: "5.4",
            name: "Loop: Previous should wrap to last",
            decode_mode: false,
            action: TestAction::Previous,
            expected: &[
                "Should wrap to last file (Sintel)",
                "Playback should continue seamlessly",
            ],
        },
    ];

    // Run tests
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut flow_id: Option<uuid::Uuid> = None;
    let block_id = "mediaplayer";

    for (i, test) in tests.iter().enumerate() {
        // Skip tests before the requested start phase
        if let Some(start_phase) = config.start_phase {
            let test_phase: u32 = test
                .id
                .split('.')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if test_phase < start_phase {
                continue;
            }
        }

        print_header(&format!("TEST {}: {}", test.id, test.name));

        // Determine if we need a new flow
        let needs_new_flow = match &test.action {
            TestAction::StartFlow
            | TestAction::StartFlowWithPlaylist(_)
            | TestAction::StartFlowWithPlaylistLoop(_)
            | TestAction::StartDecodeMode(_)
            | TestAction::StartDecodeModeLoop(_) => true,
            _ => flow_id.is_none(),
        };

        if needs_new_flow {
            // Stop and delete existing flow if any
            if let Some(id) = flow_id.take() {
                print_info("Stopping previous flow...");
                let _ = api.stop_flow(id).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = api.delete_flow(id).await;
            }

            // Clean up any stale test flows (e.g., from previous runs)
            api.delete_flows_by_prefix("__MPTEST__").await?;
            tokio::time::sleep(Duration::from_secs(1)).await; // Wait for SRT port to be released

            // Determine playlist files, loop setting, and decode mode
            let (playlist_files, loop_playlist, use_decode_mode): (Vec<String>, bool, bool) =
                match &test.action {
                    TestAction::StartFlowWithPlaylist(files) => (
                        files
                            .iter()
                            .map(|f| format!("{}/{}", config.media_dir, f))
                            .collect(),
                        false,
                        false,
                    ),
                    TestAction::StartFlowWithPlaylistLoop(files) => (
                        files
                            .iter()
                            .map(|f| format!("{}/{}", config.media_dir, f))
                            .collect(),
                        true,
                        false,
                    ),
                    TestAction::StartDecodeMode(files) => (
                        files
                            .iter()
                            .map(|f| format!("{}/{}", config.media_dir, f))
                            .collect(),
                        false,
                        true,
                    ),
                    TestAction::StartDecodeModeLoop(files) => (
                        files
                            .iter()
                            .map(|f| format!("{}/{}", config.media_dir, f))
                            .collect(),
                        true,
                        true,
                    ),
                    _ => (
                        vec![format!("{}/{}", config.media_dir, config.test_file)],
                        false,
                        false,
                    ),
                };

            // Create new flow
            let flow = if use_decode_mode {
                let loop_str = if loop_playlist { " (loop enabled)" } else { "" };
                print_info(&format!(
                    "Creating flow: MediaPlayer -> VideoEncoder -> MPEGTSSRT{}",
                    loop_str
                ));
                flow_builder::build_decode_mode_flow(
                    &format!("__MPTEST__ {}", test.id),
                    &config.srt_uri,
                    &playlist_files,
                    loop_playlist,
                )
            } else {
                let loop_str = if loop_playlist { " (loop enabled)" } else { "" };
                print_info(&format!(
                    "Creating flow: MediaPlayer -> MPEGTSSRT{}",
                    loop_str
                ));
                flow_builder::build_mediaplayer_to_srt_flow_full(
                    &format!("__MPTEST__ {}", test.id),
                    test.decode_mode,
                    &config.srt_uri,
                    &playlist_files,
                    loop_playlist,
                )
            };

            let id = api.create_flow(&flow).await?;
            flow_id = Some(id);

            print_info("Starting flow...");
            api.start_flow(id).await?;

            // Give pipeline time to start
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Execute test action
        let current_flow_id = flow_id.expect("Flow should exist");
        match &test.action {
            TestAction::StartFlow
            | TestAction::StartFlowWithPlaylist(_)
            | TestAction::StartFlowWithPlaylistLoop(_)
            | TestAction::StartDecodeMode(_)
            | TestAction::StartDecodeModeLoop(_) => {
                // Already started above
            }
            TestAction::Pause => {
                print_info("Sending pause command...");
                api.player_control(current_flow_id, block_id, "pause")
                    .await?;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            TestAction::Play => {
                print_info("Sending play command...");
                api.player_control(current_flow_id, block_id, "play")
                    .await?;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            TestAction::SeekForward(ns) => {
                // Get current position and add offset
                let state = api.get_player_state(current_flow_id, block_id).await?;
                let new_pos = state.position_ns + ns;
                print_info(&format!(
                    "Seeking from {}s to {}s...",
                    state.position_ns / 1_000_000_000,
                    new_pos / 1_000_000_000
                ));
                api.seek(current_flow_id, block_id, new_pos).await?;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            TestAction::SeekTo(ns) => {
                print_info(&format!("Seeking to {}s...", ns / 1_000_000_000));
                api.seek(current_flow_id, block_id, *ns).await?;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            TestAction::Next => {
                print_info("Sending next command...");
                match api.player_control(current_flow_id, block_id, "next").await {
                    Ok(_) => {}
                    Err(e) => {
                        // Edge case: at end of playlist - this is expected behavior
                        print_info(&format!("Next command returned: {}", e));
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await; // Give time for file switch
            }
            TestAction::Previous => {
                print_info("Sending previous command...");
                match api
                    .player_control(current_flow_id, block_id, "previous")
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        // Edge case: at start of playlist - this is expected behavior
                        print_info(&format!("Previous command returned: {}", e));
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await; // Give time for file switch
            }
            TestAction::GotoFile(index) => {
                print_info(&format!("Going to file index {}...", index));
                api.goto_file(current_flow_id, block_id, *index).await?;
                tokio::time::sleep(Duration::from_secs(2)).await; // Give time for file switch
            }
        }

        // Prompt for observation
        match prompt::prompt_observation(test.expected) {
            TestResult::Pass => {
                print_success(&format!("Test {} PASSED", test.id));
                passed += 1;
            }
            TestResult::Fail(reason) => {
                prompt::print_error(&format!("Test {} FAILED: {}", test.id, reason));
                failed += 1;
            }
            TestResult::Skip => {
                prompt::print_warning(&format!("Test {} SKIPPED", test.id));
                skipped += 1;
            }
            TestResult::Quit => {
                print_info("Test run aborted by user");
                break;
            }
        }

        // Small pause between tests
        if i < tests.len() - 1 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    // Cleanup
    if let Some(id) = flow_id {
        print_info("Cleaning up...");
        let _ = api.stop_flow(id).await;
        let _ = api.delete_flow(id).await;
    }

    // Print summary
    println!();
    print_header("TEST SUMMARY");
    print_success(&format!("Passed: {}", passed));
    if failed > 0 {
        prompt::print_error(&format!("Failed: {}", failed));
    } else {
        print_info(&format!("Failed: {}", failed));
    }
    if skipped > 0 {
        prompt::print_warning(&format!("Skipped: {}", skipped));
    }

    // Stop server
    print_info("Stopping server...");
    server.stop().await?;

    Ok(())
}
