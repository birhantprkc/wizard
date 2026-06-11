//! Best-effort VRAM/RAM detection, ported from `install.sh`'s
//! `detect_memory` / `select_model`. Used to suggest a local model (GGUF for
//! llama.cpp, tag for Ollama) that fits the machine. Everything here is
//! defensive: external commands may be absent or print garbage, so only plain
//! unsigned integers `> 0` are trusted, and total failure yields `None`.

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

/// A GGUF model tier for llama.cpp: a display name, the exact filename under
/// `~/.wizard/models/`, and where to download it from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GgufModel {
    /// Human-facing name, e.g. `"Qwen3.6 27B"`.
    pub name: &'static str,
    /// Filename under `~/.wizard/models/`, e.g. `"Qwen3.6-27B-Q4_K_M.gguf"`.
    pub file: &'static str,
    /// Hugging Face download URL for [`Self::file`].
    pub url: &'static str,
}

/// GGUF tiers (largest first), the Q4_K_M counterparts of the Ollama tags in
/// [`suggest_ollama_model`]. `install.sh` (WIZARD_LOCAL=1) and
/// [`crate::local_setup`] download these exact files.
pub const GGUF_TIERS: &[GgufModel] = &[
    GgufModel {
        name: "Qwen3.6 35B",
        file: "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.6-35B-A3B-GGUF/resolve/main/Qwen3.6-35B-A3B-UD-Q4_K_M.gguf",
    },
    GgufModel {
        name: "Qwen3.6 27B",
        file: "Qwen3.6-27B-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.6-27B-GGUF/resolve/main/Qwen3.6-27B-Q4_K_M.gguf",
    },
    GgufModel {
        name: "Qwen3.5 9B",
        file: "Qwen3.5-9B-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/main/Qwen3.5-9B-Q4_K_M.gguf",
    },
];

/// The tier whose [`GgufModel::file`] matches `file_name`, if any. Used to
/// decide whether a missing `gguf_path` is one Wizard knows how to download.
pub fn gguf_tier_for_file(file_name: &str) -> Option<&'static GgufModel> {
    GGUF_TIERS.iter().find(|tier| tier.file == file_name)
}

/// Suggest a GGUF tier for a given memory budget (GB). Same boundaries as
/// [`suggest_ollama_model`].
pub fn suggest_gguf_model(gb: u64) -> &'static GgufModel {
    if gb >= 24 {
        &GGUF_TIERS[0]
    } else if gb >= 18 {
        &GGUF_TIERS[1]
    } else {
        &GGUF_TIERS[2]
    }
}

/// Run detection and return `(tier, explanation)` for llama.cpp. Falls back to
/// the smallest tier with an explanatory note when nothing can be detected.
pub fn suggest_gguf() -> (&'static GgufModel, String) {
    match detect_memory() {
        Some(detected) => {
            let tier = suggest_gguf_model(detected.gb);
            let explanation = format!(
                "Detected {} GB of {} → {}",
                detected.gb, detected.source, tier.file
            );
            (tier, explanation)
        }
        None => {
            let tier = suggest_gguf_model(0);
            let explanation = format!(
                "Could not detect GPU VRAM or system RAM; defaulting to {}",
                tier.file
            );
            (tier, explanation)
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
    fn gguf_tier_boundaries_match_ollama_tiers() {
        assert_eq!(suggest_gguf_model(7).file, "Qwen3.5-9B-Q4_K_M.gguf");
        assert_eq!(suggest_gguf_model(17).file, "Qwen3.5-9B-Q4_K_M.gguf");
        assert_eq!(suggest_gguf_model(18).file, "Qwen3.6-27B-Q4_K_M.gguf");
        assert_eq!(suggest_gguf_model(23).file, "Qwen3.6-27B-Q4_K_M.gguf");
        assert_eq!(
            suggest_gguf_model(24).file,
            "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf"
        );
        // Every boundary picks the same tier in both tables.
        for gb in [0, 17, 18, 23, 24, 48] {
            let gguf = suggest_gguf_model(gb);
            let tag = suggest_ollama_model(gb);
            // "qwen3.6:27b" ↔ "Qwen3.6 27B": compare the size suffix.
            let size = tag.split(':').nth(1).unwrap().to_uppercase();
            assert!(
                gguf.name.ends_with(&size),
                "tier mismatch at {gb} GB: {tag} vs {}",
                gguf.name
            );
        }
    }

    #[test]
    fn gguf_tier_urls_end_with_their_file_names() {
        for tier in GGUF_TIERS {
            assert!(
                tier.url.ends_with(tier.file),
                "URL/file mismatch for {}: {}",
                tier.name,
                tier.url
            );
            assert!(tier.url.starts_with("https://"));
        }
        assert_eq!(
            gguf_tier_for_file("Qwen3.5-9B-Q4_K_M.gguf").map(|t| t.name),
            Some("Qwen3.5 9B")
        );
        assert_eq!(gguf_tier_for_file("other.gguf"), None);
    }

    #[test]
    fn suggest_gguf_returns_a_known_tier() {
        let (tier, explanation) = suggest_gguf();
        assert!(GGUF_TIERS.contains(tier), "unexpected tier {tier:?}");
        assert!(explanation.contains(tier.file));
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
