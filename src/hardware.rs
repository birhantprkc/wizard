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

/// `MemTotal` in GB from `/proc/meminfo` contents.
fn parse_meminfo_total_gb(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    // Format: "MemTotal:       16312456 kB"
    let kb = line.split_whitespace().nth(1).and_then(parse_positive)?;
    let gb = kb / (1024 * 1024);
    (gb > 0).then_some(gb)
}

/// A cgroup memory limit in bytes from the raw file contents. `None` for
/// "no limit": cgroup v2 spells that `max`, cgroup v1 reports
/// `PAGE_COUNTER_MAX` (~`LONG_MAX`, far beyond any real machine).
fn parse_cgroup_limit_bytes(contents: &str) -> Option<u64> {
    /// Anything this large (1 EiB) is a no-limit sentinel, not a limit.
    const NO_LIMIT: u64 = 1 << 60;
    match contents.trim() {
        "max" => None,
        raw => parse_positive(raw).filter(|&bytes| bytes < NO_LIMIT),
    }
}

/// The cgroup memory limit confining this process, in GB. Checks cgroup v2
/// (`memory.max`) then v1 (`memory.limit_in_bytes`); `None` outside
/// containers, or when no readable file carries a real limit.
fn cgroup_limit_gb() -> Option<u64> {
    [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .filter_map(|raw| parse_cgroup_limit_bytes(&raw))
    .min()
    .map(|bytes| bytes / (1024 * 1024 * 1024))
}

/// Cap a `MemTotal` reading with an optional cgroup limit. The bool is true
/// when the limit is what set the number.
fn cap_to_cgroup(total_gb: u64, limit_gb: Option<u64>) -> (u64, bool) {
    match limit_gb {
        Some(limit) if limit < total_gb => (limit, true),
        _ => (total_gb, false),
    }
}

/// Total system RAM in GB from `/proc/meminfo` (`MemTotal`, kB), capped by
/// the cgroup memory limit when one is smaller — in a container `MemTotal`
/// reports the host's RAM, not what this process may actually use. The bool
/// is true when the cgroup limit won.
fn system_ram_gb() -> Option<(u64, bool)> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let total_gb = parse_meminfo_total_gb(&meminfo)?;
    let (gb, capped) = cap_to_cgroup(total_gb, cgroup_limit_gb());
    (gb > 0).then_some((gb, capped))
}

/// System RAM usable by this process, in GB ([`system_ram_gb`] including any
/// cgroup cap). This is what a spawned llama-server can actually allocate:
/// [`crate::server::spawn`] passes no GPU-offload flags, so the weights load
/// into system memory even on GPU machines.
pub fn usable_ram_gb() -> Option<u64> {
    system_ram_gb().map(|(gb, _)| gb)
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
    if let Some((gb, capped)) = system_ram_gb() {
        return Some(Detected {
            gb,
            source: if capped {
                "system RAM (cgroup limit)".to_string()
            } else {
                "system RAM (no GPU detected)".to_string()
            },
        });
    }
    None
}

/// Whether [`detect_memory`] found GPU VRAM rather than falling back to system
/// RAM. The local model tier is sized to the detected budget, so a `true` here
/// means a VRAM-tiered model was picked and the spawned `llama-server` must
/// offload to the GPU — otherwise it loads entirely into RAM and a large model
/// OOMs the host during startup.
pub fn has_gpu() -> bool {
    detect_memory().is_some_and(|detected| detected.source.starts_with("GPU VRAM"))
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
    /// Approximate file size in GB, used to refuse a model that cannot fit
    /// in RAM before downloading it.
    pub approx_gb: u64,
}

/// GGUF tiers (largest first), the Q4_K_M counterparts of the Ollama tags in
/// [`suggest_ollama_model`]. `install.sh` (WIZARD_LOCAL=1) and
/// [`crate::local_setup`] download these exact files.
pub const GGUF_TIERS: &[GgufModel] = &[
    GgufModel {
        name: "Qwen3.6 35B",
        file: "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.6-35B-A3B-GGUF/resolve/main/Qwen3.6-35B-A3B-UD-Q4_K_M.gguf",
        approx_gb: 20,
    },
    GgufModel {
        name: "Qwen3.6 27B",
        file: "Qwen3.6-27B-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.6-27B-GGUF/resolve/main/Qwen3.6-27B-Q4_K_M.gguf",
        approx_gb: 16,
    },
    GgufModel {
        name: "Qwen3.5 9B",
        file: "Qwen3.5-9B-Q4_K_M.gguf",
        url: "https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/main/Qwen3.5-9B-Q4_K_M.gguf",
        approx_gb: 6,
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

/// Memory budget for the GGUF tier choice: a GPU VRAM reading is capped by
/// system RAM, because the spawned llama-server runs without GPU offload
/// (no `-ngl`; [`crate::server::spawn`]) and the prebuilts are CPU/Vulkan —
/// the weights must fit in RAM regardless of VRAM. The bool is true when
/// the RAM cap won.
fn gguf_budget_gb(detected: &Detected, ram_gb: Option<u64>) -> (u64, bool) {
    match ram_gb {
        Some(ram) if detected.source.starts_with("GPU VRAM") && ram < detected.gb => (ram, true),
        _ => (detected.gb, false),
    }
}

/// Run detection and return `(tier, explanation)` for llama.cpp. Falls back to
/// the smallest tier with an explanatory note when nothing can be detected.
pub fn suggest_gguf() -> (&'static GgufModel, String) {
    match detect_memory() {
        Some(detected) => {
            let (budget, ram_capped) = gguf_budget_gb(&detected, usable_ram_gb());
            let tier = suggest_gguf_model(budget);
            let explanation = if ram_capped {
                format!(
                    "Detected {} GB of {}, capped by {budget} GB of system RAM → {}",
                    detected.gb, detected.source, tier.file
                )
            } else {
                format!(
                    "Detected {} GB of {} → {}",
                    detected.gb, detected.source, tier.file
                )
            };
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
    fn parse_meminfo_total_gb_reads_memtotal() {
        let meminfo = "MemTotal:       16312456 kB\nMemFree:         1234 kB\n";
        assert_eq!(parse_meminfo_total_gb(meminfo), Some(15));
        assert_eq!(parse_meminfo_total_gb("MemFree: 1234 kB\n"), None);
        assert_eq!(parse_meminfo_total_gb("MemTotal: garbage kB\n"), None);
        assert_eq!(parse_meminfo_total_gb(""), None);
    }

    #[test]
    fn parse_cgroup_limit_rejects_no_limit_sentinels() {
        // cgroup v2 spells "no limit" as the literal string `max`.
        assert_eq!(parse_cgroup_limit_bytes("max\n"), None);
        // cgroup v1 reports PAGE_COUNTER_MAX (~LONG_MAX) when unconfined.
        assert_eq!(parse_cgroup_limit_bytes("9223372036854771712\n"), None);
        assert_eq!(parse_cgroup_limit_bytes(&(1u64 << 60).to_string()), None);
        // A real limit (12 GiB, like a Colab container) is taken at face value.
        assert_eq!(
            parse_cgroup_limit_bytes("12884901888\n"),
            Some(12_884_901_888)
        );
        assert_eq!(parse_cgroup_limit_bytes("0"), None);
        assert_eq!(parse_cgroup_limit_bytes("not a number"), None);
        assert_eq!(parse_cgroup_limit_bytes(""), None);
    }

    #[test]
    fn cap_to_cgroup_only_lowers() {
        assert_eq!(cap_to_cgroup(64, Some(12)), (12, true), "container cap");
        assert_eq!(cap_to_cgroup(16, Some(32)), (16, false), "limit above RAM");
        assert_eq!(cap_to_cgroup(16, Some(16)), (16, false), "equal is no cap");
        assert_eq!(cap_to_cgroup(16, None), (16, false), "no limit");
    }

    #[test]
    fn gguf_budget_caps_vram_by_system_ram() {
        let gpu = Detected {
            gb: 24,
            source: "GPU VRAM (nvidia-smi)".to_string(),
        };
        // No -ngl is passed when spawning, so RAM is the binding constraint.
        assert_eq!(gguf_budget_gb(&gpu, Some(12)), (12, true));
        assert_eq!(gguf_budget_gb(&gpu, Some(64)), (24, false));
        assert_eq!(gguf_budget_gb(&gpu, None), (24, false));
        let ram = Detected {
            gb: 12,
            source: "system RAM (cgroup limit)".to_string(),
        };
        assert_eq!(gguf_budget_gb(&ram, Some(12)), (12, false));
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
