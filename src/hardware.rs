//! Best-effort VRAM/RAM detection, ported from `install.sh`'s
//! `detect_memory` / `select_model`. Used to suggest an Ollama model that fits
//! the machine. Everything here is defensive: external commands may be absent
//! or print garbage, so only plain unsigned integers `> 0` are trusted, and
//! total failure yields `None`.

use std::process::Command;

/// Detected memory budget and where the number came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Available memory in gibibytes (GPU VRAM, or system RAM as a fallback).
    pub gb: u64,
    /// Human description of the source (e.g. `"GPU VRAM (nvidia-smi)"`).
    pub source: String,
}

/// Parse a line as a plain unsigned integer `> 0`, ignoring surrounding
/// whitespace. Returns `None` for anything else (headers, units, blanks).
fn parse_positive(line: &str) -> Option<u64> {
    match line.trim().parse::<u64>() {
        Ok(value) if value > 0 => Some(value),
        _ => None,
    }
}

/// Run a command and capture stdout as a string, or `None` if it cannot run or
/// exits non-zero.
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Largest GPU VRAM in GB via `nvidia-smi` (reports MiB).
fn nvidia_vram_gb() -> Option<u64> {
    let stdout = command_stdout(
        "nvidia-smi",
        &["--query-gpu=memory.total", "--format=csv,noheader,nounits"],
    )?;
    let mib = stdout.lines().filter_map(parse_positive).max()?;
    let gb = mib / 1024;
    (gb > 0).then_some(gb)
}

/// Largest GPU VRAM in GB via `rocm-smi` (reports bytes).
fn rocm_vram_gb() -> Option<u64> {
    let stdout = command_stdout("rocm-smi", &["--showmeminfo", "vram", "--csv"])?;
    // The CSV mixes labels and byte counts; trust the largest plausible
    // integer found in any comma-separated field.
    let bytes = stdout
        .lines()
        .flat_map(|line| line.split(','))
        .filter_map(parse_positive)
        .max()?;
    let gb = bytes / (1024 * 1024 * 1024);
    (gb > 0).then_some(gb)
}

/// Largest GPU VRAM in GB from sysfs (`mem_info_vram_total`, bytes).
fn sysfs_vram_gb() -> Option<u64> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    let max_bytes = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("card"))
        })
        .filter_map(|entry| {
            let path = entry.path().join("device/mem_info_vram_total");
            std::fs::read_to_string(path)
                .ok()
                .and_then(|raw| parse_positive(&raw))
        })
        .max()?;
    let gb = max_bytes / (1024 * 1024 * 1024);
    (gb > 0).then_some(gb)
}

/// Total system RAM in GB from `/proc/meminfo` (`MemTotal`, kB).
fn system_ram_gb() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    // Format: "MemTotal:       16312456 kB"
    let kb = line.split_whitespace().nth(1).and_then(parse_positive)?;
    let gb = kb / (1024 * 1024);
    (gb > 0).then_some(gb)
}

/// Detect the memory budget, preferring GPU VRAM (nvidia → rocm → sysfs) and
/// falling back to system RAM. `None` only when everything fails.
pub fn detect_memory() -> Option<Detected> {
    if let Some(gb) = nvidia_vram_gb() {
        return Some(Detected {
            gb,
            source: "GPU VRAM (nvidia-smi)".to_string(),
        });
    }
    if let Some(gb) = rocm_vram_gb() {
        return Some(Detected {
            gb,
            source: "GPU VRAM (rocm-smi)".to_string(),
        });
    }
    if let Some(gb) = sysfs_vram_gb() {
        return Some(Detected {
            gb,
            source: "GPU VRAM (sysfs)".to_string(),
        });
    }
    if let Some(gb) = system_ram_gb() {
        return Some(Detected {
            gb,
            source: "system RAM (no GPU detected)".to_string(),
        });
    }
    None
}

/// Suggest an Ollama model tag for a given memory budget (GB). Mirrors
/// `install.sh`'s tiers.
pub fn suggest_ollama_model(gb: u64) -> &'static str {
    if gb >= 24 {
        "qwen3.6:35b"
    } else if gb >= 18 {
        "qwen3.6:27b"
    } else {
        "qwen3.5:9b"
    }
}

/// Run detection and return `(model, explanation)`. Falls back to the smallest
/// model with an explanatory note when nothing can be detected.
pub fn suggest_model() -> (String, String) {
    match detect_memory() {
        Some(detected) => {
            let model = suggest_ollama_model(detected.gb);
            let explanation = format!(
                "Detected {} GB of {} → {}",
                detected.gb, detected.source, model
            );
            (model.to_string(), explanation)
        }
        None => {
            let model = suggest_ollama_model(0);
            let explanation =
                format!("Could not detect GPU VRAM or system RAM; defaulting to {model}");
            (model.to_string(), explanation)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_tier_boundaries() {
        assert_eq!(suggest_ollama_model(7), "qwen3.5:9b");
        assert_eq!(suggest_ollama_model(8), "qwen3.5:9b");
        assert_eq!(suggest_ollama_model(17), "qwen3.5:9b");
        assert_eq!(suggest_ollama_model(18), "qwen3.6:27b");
        assert_eq!(suggest_ollama_model(23), "qwen3.6:27b");
        assert_eq!(suggest_ollama_model(24), "qwen3.6:35b");
    }

    #[test]
    fn parse_positive_rejects_non_positive_integers() {
        assert_eq!(parse_positive("  42 "), Some(42));
        assert_eq!(parse_positive("0"), None);
        assert_eq!(parse_positive("-1"), None);
        assert_eq!(parse_positive("12 kB"), None);
        assert_eq!(parse_positive("MemTotal:"), None);
        assert_eq!(parse_positive(""), None);
    }

    #[test]
    fn suggest_model_returns_a_known_tag() {
        let (model, explanation) = suggest_model();
        assert!(
            ["qwen3.6:35b", "qwen3.6:27b", "qwen3.5:9b"].contains(&model.as_str()),
            "unexpected model {model}"
        );
        assert!(explanation.contains(&model));
    }
}
