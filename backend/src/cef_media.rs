//! The camera and microphone an HTML source's page is given.
//!
//! A page renders to video and has no business with the server's own devices,
//! which on a broadcast server may be capture cards. Some pages will not start
//! without a camera and a microphone, though: a video call joined to be
//! watched, for one. Chromium can replace every device with a synthetic one
//! (`use-fake-device-for-media-stream`), and Strom's gstcefsrc grants a page
//! camera and microphone only when it does.
//!
//! The synthetic devices play a file each. Chromium's own default is a beep
//! every second and a green test card, which the other people on a call would
//! hear and see, so Strom writes silence and a black frame instead.

use std::io;
use std::path::{Path, PathBuf};

/// Sample rate of the silence, in Hz.
const SAMPLE_RATE: u32 = 48_000;

/// Size of the black frame. Small: nobody is meant to look at it.
const WIDTH: usize = 640;
const HEIGHT: usize = 360;

/// One second of 16-bit mono silence, as a WAV file. Chromium loops it.
fn silence_wav() -> Vec<u8> {
    let data_len = SAMPLE_RATE * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0);
    wav
}

/// One black frame, as a YUV4MPEG2 file. Chromium loops it.
fn black_y4m() -> Vec<u8> {
    let header = format!("YUV4MPEG2 W{WIDTH} H{HEIGHT} F1:1 Ip A1:1 C420\n");
    let luma = WIDTH * HEIGHT;
    let chroma = (WIDTH / 2) * (HEIGHT / 2);
    let mut y4m = Vec::with_capacity(header.len() + 6 + luma + 2 * chroma);
    y4m.extend_from_slice(header.as_bytes());
    y4m.extend_from_slice(b"FRAME\n");
    // Black in limited range: Y at 16, U and V at their midpoint.
    y4m.resize(y4m.len() + luma, 16);
    y4m.resize(y4m.len() + 2 * chroma, 128);
    y4m
}

/// The switch that replaces every camera and microphone with a synthetic one.
pub const FAKE_DEVICE_SWITCH: &str = "use-fake-device-for-media-stream";

/// Write the silence and the black frame into `dir`, and return the
/// Chromium switches that make them the page's only devices.
pub fn fake_device_switches(dir: &Path) -> io::Result<Vec<String>> {
    std::fs::create_dir_all(dir)?;
    let audio: PathBuf = dir.join("silence.wav");
    let video: PathBuf = dir.join("black.y4m");
    std::fs::write(&audio, silence_wav())?;
    std::fs::write(&video, black_y4m())?;
    Ok(vec![
        FAKE_DEVICE_SWITCH.to_string(),
        format!("use-file-for-fake-audio-capture={}", audio.display()),
        format!("use-file-for-fake-video-capture={}", video.display()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_silence_is_a_well_formed_silent_wav() {
        let wav = silence_wav();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        let riff_len = u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff_len + 8, wav.len());
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(data_len + 44, wav.len());
        assert!(wav[44..].iter().all(|&b| b == 0), "silence is all zero");
    }

    #[test]
    fn the_frame_is_one_black_420_frame() {
        let y4m = black_y4m();
        let header_end = y4m.iter().position(|&b| b == b'\n').unwrap() + 1;
        let header = std::str::from_utf8(&y4m[..header_end]).unwrap();
        assert!(header.starts_with("YUV4MPEG2 W640 H360 "));
        assert!(header.contains(" C420"));
        let frame = &y4m[header_end..];
        assert!(frame.starts_with(b"FRAME\n"));
        let planes = &frame[6..];
        assert_eq!(planes.len(), WIDTH * HEIGHT * 3 / 2);
        assert!(planes[..WIDTH * HEIGHT].iter().all(|&b| b == 16));
        assert!(planes[WIDTH * HEIGHT..].iter().all(|&b| b == 128));
    }

    #[test]
    fn the_switches_point_at_the_files_written() {
        let dir = tempfile::tempdir().unwrap();
        let switches = fake_device_switches(dir.path()).unwrap();
        assert_eq!(switches[0], "use-fake-device-for-media-stream");
        for (switch, file) in [(&switches[1], "silence.wav"), (&switches[2], "black.y4m")] {
            let path = switch.split_once('=').unwrap().1;
            assert!(path.ends_with(file));
            assert!(std::path::Path::new(path).is_file());
        }
    }
}
