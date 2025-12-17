//! Interactive prompts for human verification
//!
//! Provides colored terminal output and user input handling.

use std::io::{self, Write};

use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::ExecutableCommand;

/// Result of a test observation
pub enum TestResult {
    Pass,
    Fail(String),
    Skip,
    Quit,
}

/// Print the main banner
pub fn print_banner() {
    let mut stdout = io::stdout();
    println!();
    let _ = stdout.execute(SetForegroundColor(Color::Cyan));
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║  MEDIA PLAYER TEST HARNESS                               ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    let _ = stdout.execute(ResetColor);
    println!();
}

/// Print a section header
pub fn print_header(text: &str) {
    let mut stdout = io::stdout();
    println!();
    let _ = stdout.execute(SetForegroundColor(Color::Cyan));
    println!("═══════════════════════════════════════════════════════════");
    println!("{}", text);
    println!("═══════════════════════════════════════════════════════════");
    let _ = stdout.execute(ResetColor);
    println!();
}

/// Print an info message
pub fn print_info(text: &str) {
    let mut stdout = io::stdout();
    let _ = stdout.execute(SetForegroundColor(Color::Blue));
    let _ = stdout.execute(Print(format!("[TEST] {}\n", text)));
    let _ = stdout.execute(ResetColor);
}

/// Print a success message
pub fn print_success(text: &str) {
    let mut stdout = io::stdout();
    let _ = stdout.execute(SetForegroundColor(Color::Green));
    let _ = stdout.execute(Print(format!("[TEST] ✓ {}\n", text)));
    let _ = stdout.execute(ResetColor);
}

/// Print an error message
pub fn print_error(text: &str) {
    let mut stdout = io::stdout();
    let _ = stdout.execute(SetForegroundColor(Color::Red));
    let _ = stdout.execute(Print(format!("[TEST] ✗ {}\n", text)));
    let _ = stdout.execute(ResetColor);
}

/// Print a warning message
pub fn print_warning(text: &str) {
    let mut stdout = io::stdout();
    let _ = stdout.execute(SetForegroundColor(Color::Yellow));
    let _ = stdout.execute(Print(format!("[TEST] ⚠ {}\n", text)));
    let _ = stdout.execute(ResetColor);
}

/// Wait for user to press Enter
pub fn wait_for_enter(prompt: &str) {
    let mut stdout = io::stdout();
    let _ = stdout.execute(SetForegroundColor(Color::Magenta));
    print!("{}", prompt);
    let _ = stdout.execute(ResetColor);
    let _ = io::stdout().flush();

    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
}

/// Prompt user to verify an expected observation
pub fn prompt_observation(expected: &[&str]) -> TestResult {
    let mut stdout = io::stdout();

    // Print expected observation box
    let _ = stdout.execute(SetForegroundColor(Color::White));
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ EXPECTED OBSERVATION:                                   │");
    for exp in expected {
        println!("│   • {:<53} │", exp);
    }
    println!("└─────────────────────────────────────────────────────────┘");
    let _ = stdout.execute(ResetColor);
    println!();

    // Prompt for result
    loop {
        let _ = stdout.execute(SetForegroundColor(Color::Magenta));
        print!("Did you observe the expected behavior? [Y]es/[n]o/[s]kip/[q]uit (Enter=Yes): ");
        let _ = stdout.execute(ResetColor);
        let _ = io::stdout().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            continue;
        }

        let input = input.trim().to_lowercase();
        match input.as_str() {
            "" | "y" | "yes" => return TestResult::Pass, // Enter defaults to Yes
            "n" | "no" => {
                // Ask for failure description
                let _ = stdout.execute(SetForegroundColor(Color::Yellow));
                print!("Please describe what you observed: ");
                let _ = stdout.execute(ResetColor);
                let _ = io::stdout().flush();

                let mut reason = String::new();
                let _ = io::stdin().read_line(&mut reason);
                return TestResult::Fail(reason.trim().to_string());
            }
            "s" | "skip" => return TestResult::Skip,
            "q" | "quit" => return TestResult::Quit,
            _ => {
                print_warning("Invalid input. Please enter y, n, s, or q.");
            }
        }
    }
}
