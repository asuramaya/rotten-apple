//! Deep host hardware spec — RAM modules, GPU VRAM, etc.
//!
//! Best-effort. Each probe is independently fallible — if dmidecode
//! isn't installed, or we're not root, or the GPU has no driver
//! loaded, the corresponding field comes back empty. None of it
//! blocks startup.
//!
//! Intentionally probed ONCE at cockpit startup and cached on `State`,
//! since some of the calls (dmidecode, nvidia-smi) spawn processes
//! that are too expensive to run on every redraw.

use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct HostSpecs {
    pub memory_modules: Vec<MemoryModule>,
    pub gpu_specs: Vec<GpuSpec>,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryModule {
    pub locator: String,
    pub size_gb: Option<u64>,
    pub kind: Option<String>,           // "DDR4", "DDR5", "LPDDR5", …
    pub speed_mts: Option<u32>,         // configured speed, MT/s
    pub manufacturer: Option<String>,
    pub part_number: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GpuSpec {
    /// BDF in `0000:01:00.0` shape so the cockpit can match it back to
    /// the [`crate::GpuDevice`] enumeration.
    pub bdf: String,
    pub model: Option<String>,
    pub vram_mb: Option<u64>,
}

impl HostSpecs {
    pub fn probe() -> Self {
        Self {
            memory_modules: probe_memory_modules(),
            gpu_specs: probe_gpu_specs(),
        }
    }

    /// Total installed RAM across all DIMMs, GiB. None when DMI didn't
    /// give us anything (no dmidecode, not root, …).
    pub fn total_memory_gb(&self) -> Option<u64> {
        let total: u64 = self.memory_modules.iter()
            .filter_map(|m| m.size_gb)
            .sum();
        if total == 0 { None } else { Some(total) }
    }

    /// Best one-line summary of installed memory: "32 GiB DDR5 @ 4800 MT/s
    /// (1× 32 GB DIMM 1)". Empty string when we have no DMI data.
    pub fn memory_summary(&self) -> String {
        if self.memory_modules.is_empty() { return String::new() }
        let total = self.total_memory_gb().unwrap_or(0);
        let kind = self.memory_modules.iter()
            .filter_map(|m| m.kind.clone())
            .next()
            .unwrap_or_default();
        let speed = self.memory_modules.iter()
            .filter_map(|m| m.speed_mts)
            .next();
        let count = self.memory_modules.iter().filter(|m| m.size_gb.is_some()).count();
        let speed_part = speed.map(|s| format!(" @ {s} MT/s")).unwrap_or_default();
        format!("{total} GiB {kind}{speed_part} ({count}× DIMM)")
    }
}

fn probe_memory_modules() -> Vec<MemoryModule> {
    let Ok(out) = Command::new("dmidecode").args(["--type", "17"]).output()
    else { return Vec::new() };
    if !out.status.success() { return Vec::new() }
    parse_dmidecode_type17(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the `dmidecode --type 17` (Memory Device) output. Each
/// "Handle 0x...." block is one DIMM slot; `Size: No Module Installed`
/// blocks are skipped. Field shape is stable across dmidecode versions.
pub fn parse_dmidecode_type17(s: &str) -> Vec<MemoryModule> {
    let mut out = Vec::new();
    let mut current: Option<MemoryModule> = None;
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed == "Memory Device" {
            if let Some(m) = current.take()
                && m.size_gb.is_some()
            {
                out.push(m);
            }
            current = Some(MemoryModule::default());
            continue;
        }
        let Some(m) = current.as_mut() else { continue };
        let Some((k, v)) = trimmed.split_once(": ") else { continue };
        let v = v.trim();
        match k.trim() {
            "Locator"     => m.locator = v.to_string(),
            "Size" => {
                // "32 GB" / "16384 MB" / "No Module Installed"
                if v == "No Module Installed" || v == "Unknown" { continue }
                let mut parts = v.split_whitespace();
                if let (Some(n), Some(u)) = (parts.next(), parts.next())
                    && let Ok(n) = n.parse::<u64>()
                {
                    m.size_gb = Some(match u {
                        "GB" => n,
                        "MB" => n / 1024,
                        _    => continue,
                    });
                }
            }
            "Type" if v != "Unknown" => m.kind = Some(v.to_string()),
            "Configured Memory Speed" | "Speed" => {
                // "4800 MT/s" / "Unknown"
                if v == "Unknown" { continue }
                if let Some(n) = v.split_whitespace().next()
                    && let Ok(n) = n.parse::<u32>()
                {
                    m.speed_mts = Some(n);
                }
            }
            "Manufacturer" if !v.is_empty() && v != "Unknown" => {
                m.manufacturer = Some(v.to_string());
            }
            "Part Number" => {
                let trimmed = v.trim();
                if !trimmed.is_empty() && trimmed != "Unknown" {
                    m.part_number = Some(trimmed.to_string());
                }
            }
            _ => {}
        }
    }
    if let Some(m) = current
        && m.size_gb.is_some()
    {
        out.push(m);
    }
    out
}

fn probe_gpu_specs() -> Vec<GpuSpec> {
    let mut out = Vec::new();
    out.extend(probe_nvidia_specs());
    out.extend(probe_amd_specs());
    out
}

fn probe_nvidia_specs() -> Vec<GpuSpec> {
    let Ok(o) = Command::new("nvidia-smi")
        .args(["--query-gpu=pci.bus_id,name,memory.total",
               "--format=csv,noheader,nounits"])
        .output()
    else { return Vec::new() };
    if !o.status.success() { return Vec::new() }
    parse_nvidia_smi_csv(&String::from_utf8_lossy(&o.stdout))
}

/// Parse `nvidia-smi --query-gpu=pci.bus_id,name,memory.total --format=csv,noheader,nounits`.
/// pci.bus_id format is `00000000:01:00.0`; we normalise to `0000:01:00.0`
/// to match Linux sysfs / xl convention.
pub fn parse_nvidia_smi_csv(s: &str) -> Vec<GpuSpec> {
    s.lines()
        .filter_map(|l| {
            let mut parts = l.split(',').map(str::trim);
            let raw_bdf = parts.next()?;
            let name = parts.next()?;
            let mb_str = parts.next()?;
            let bdf = normalise_nvidia_bdf(raw_bdf)?;
            let vram_mb = mb_str.parse::<u64>().ok();
            Some(GpuSpec {
                bdf,
                model: if name.is_empty() { None } else { Some(name.to_string()) },
                vram_mb,
            })
        })
        .collect()
}

fn normalise_nvidia_bdf(raw: &str) -> Option<String> {
    // "00000000:01:00.0" → "0000:01:00.0" (Linux uses 4 domain hex)
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() != 3 { return None }
    let domain = parts[0].trim_start_matches('0');
    let domain = if domain.is_empty() { "0".to_string() } else { domain.to_string() };
    let domain = format!("{domain:0>4}");
    Some(format!("{domain}:{}:{}", parts[1], parts[2]))
}

fn probe_amd_specs() -> Vec<GpuSpec> {
    // /sys/class/drm/card*/device/mem_info_vram_total — bytes.
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        // Match bare card0/card1/… (skip "card0-DP-1" connector entries).
        if !s.starts_with("card") || s.contains('-') { continue }
        let dev = entry.path().join("device");
        let bdf = match std::fs::read_link(&dev) {
            Ok(p) => p.file_name()
                .and_then(|n| n.to_str())
                .map(str::to_owned),
            Err(_) => None,
        };
        let Some(bdf) = bdf else { continue };
        // Skip non-AMD: AMD path exposes mem_info_vram_total; Intel/Nvidia
        // don't (Intel is UMA so no separate VRAM, NVIDIA we do via smi).
        let Ok(raw) = std::fs::read_to_string(dev.join("mem_info_vram_total"))
        else { continue };
        let bytes: u64 = raw.trim().parse().ok().unwrap_or(0);
        if bytes == 0 { continue }
        out.push(GpuSpec {
            bdf,
            model: None,
            vram_mb: Some(bytes / 1024 / 1024),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DMI: &str = "\
# dmidecode 3.6
Getting SMBIOS data from sysfs.
SMBIOS 3.4 present.

Handle 0x1100, DMI type 17, 92 bytes
Memory Device
\tArray Handle: 0x1000
\tError Information Handle: Not Provided
\tTotal Width: 64 bits
\tData Width: 64 bits
\tSize: 32 GB
\tForm Factor: SODIMM
\tSet: None
\tLocator: DIMM 1
\tBank Locator: BANK 0
\tType: DDR5
\tType Detail: Synchronous
\tSpeed: 4800 MT/s
\tManufacturer: 80AD000080AD
\tSerial Number: 7650DB63
\tAsset Tag: 01225100
\tPart Number: HMCG88MEBSA095N
\tRank: 2
\tConfigured Memory Speed: 4800 MT/s

Handle 0x1101, DMI type 17, 92 bytes
Memory Device
\tArray Handle: 0x1000
\tSize: No Module Installed
\tForm Factor: SODIMM
\tLocator: DIMM 2
\tType: Unknown
";

    #[test]
    fn parse_dmidecode_picks_up_installed_dimms_only() {
        let mods = parse_dmidecode_type17(SAMPLE_DMI);
        assert_eq!(mods.len(), 1, "only one DIMM is populated");
        let m = &mods[0];
        assert_eq!(m.locator, "DIMM 1");
        assert_eq!(m.size_gb, Some(32));
        assert_eq!(m.kind.as_deref(), Some("DDR5"));
        assert_eq!(m.speed_mts, Some(4800));
        // Configured Memory Speed should win over Speed when both present;
        // our parser overwrites (last one wins). Either is fine — the
        // value is the same on healthy hardware.
        assert_eq!(m.part_number.as_deref(), Some("HMCG88MEBSA095N"));
    }

    #[test]
    fn host_specs_summary_renders_cleanly() {
        let specs = HostSpecs {
            memory_modules: parse_dmidecode_type17(SAMPLE_DMI),
            gpu_specs: vec![],
        };
        let s = specs.memory_summary();
        assert!(s.contains("32 GiB"), "summary={s}");
        assert!(s.contains("DDR5"), "summary={s}");
        assert!(s.contains("4800"), "summary={s}");
    }

    #[test]
    fn host_specs_total_memory_aggregates() {
        let specs = HostSpecs {
            memory_modules: vec![
                MemoryModule { size_gb: Some(16), ..Default::default() },
                MemoryModule { size_gb: Some(16), ..Default::default() },
            ],
            gpu_specs: vec![],
        };
        assert_eq!(specs.total_memory_gb(), Some(32));
    }

    #[test]
    fn parse_nvidia_smi_normalises_bdf() {
        let csv = "00000000:01:00.0, NVIDIA RTX A3000 12GB Laptop GPU, 12288\n";
        let specs = parse_nvidia_smi_csv(csv);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].bdf, "0000:01:00.0");
        assert_eq!(specs[0].model.as_deref(),
            Some("NVIDIA RTX A3000 12GB Laptop GPU"));
        assert_eq!(specs[0].vram_mb, Some(12288));
    }

    #[test]
    fn parse_nvidia_smi_handles_multiple_gpus() {
        let csv = "\
00000000:01:00.0, NVIDIA RTX 4090, 24576
00000000:02:00.0, NVIDIA RTX 4090, 24576";
        let specs = parse_nvidia_smi_csv(csv);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].vram_mb, Some(24576));
    }

    #[test]
    fn host_specs_empty_summary_when_no_modules() {
        let specs = HostSpecs::default();
        assert!(specs.memory_summary().is_empty());
        assert_eq!(specs.total_memory_gb(), None);
    }
}
