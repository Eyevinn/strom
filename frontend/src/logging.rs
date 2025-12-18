//! Logging types for pipeline messages.

use egui::Color32;

/// Log message severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Informational message
    Info,
    /// Warning message
    Warning,
    /// Error message
    Error,
}

/// A log entry for pipeline messages
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Timestamp when the message was received
    pub timestamp: instant::Instant,
    /// Severity level
    pub level: LogLevel,
    /// The message content
    pub message: String,
    /// Optional source element that generated the message
    pub source: Option<String>,
    /// Optional flow ID this message relates to
    pub flow_id: Option<strom_types::FlowId>,
}

impl LogEntry {
    /// Create a new log entry
    pub fn new(
        level: LogLevel,
        message: String,
        source: Option<String>,
        flow_id: Option<strom_types::FlowId>,
    ) -> Self {
        Self {
            timestamp: instant::Instant::now(),
            level,
            message,
            source,
            flow_id,
        }
    }

    /// Get the color for this log level
    pub fn color(&self) -> Color32 {
        match self.level {
            LogLevel::Info => Color32::from_rgb(100, 180, 255),
            LogLevel::Warning => Color32::from_rgb(255, 200, 50),
            LogLevel::Error => Color32::from_rgb(255, 80, 80),
        }
    }

    /// Get the icon/prefix for this log level
    pub fn prefix(&self) -> &'static str {
        match self.level {
            LogLevel::Info => "ℹ",
            LogLevel::Warning => "⚠",
            LogLevel::Error => "✖",
        }
    }
}
