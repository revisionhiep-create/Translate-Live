use std::mem::size_of;
use std::time::Instant;
use windows::core::Interface;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

#[derive(Debug, Clone)]
pub struct TelemetrySnapshot {
    pub gpu_name: String,
    pub cpu_pct: f32,
    pub ram_mb: f32,
    pub gpu_mb: f32,
}

impl Default for TelemetrySnapshot {
    fn default() -> Self {
        Self {
            gpu_name: "Detecting GPU...".to_string(),
            cpu_pct: 0.0,
            ram_mb: 0.0,
            gpu_mb: 0.0,
        }
    }
}

pub struct TelemetrySampler {
    num_cpus: f64,
    prev_wall: Instant,
    prev_proc_100ns: u64,
    gpu_name: String,
    adapter3: Option<IDXGIAdapter3>,
    last_snapshot: TelemetrySnapshot,
}

fn filetime_to_100ns(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

fn detect_primary_dxgi_adapter() -> (String, Option<IDXGIAdapter3>) {
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return ("CPU Fallback".to_string(), None);
        };
        let mut best: Option<(usize, String, IDXGIAdapter3)> = None;
        let mut idx = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(idx) {
            idx += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            if (desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) != 0 {
                continue;
            }
            let end = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..end])
                .trim()
                .to_string();
            let vram = desc.DedicatedVideoMemory;
            if let Ok(a3) = adapter.cast::<IDXGIAdapter3>() {
                if best.as_ref().map_or(true, |(bv, _, _)| vram > *bv) {
                    best = Some((vram, name, a3));
                }
            }
        }
        match best {
            Some((_, name, a3)) => (name, Some(a3)),
            None => ("CPU Fallback".to_string(), None),
        }
    }
}

impl TelemetrySampler {
    pub fn new() -> Self {
        let num_cpus = unsafe {
            let mut si = SYSTEM_INFO::default();
            GetSystemInfo(&mut si);
            (si.dwNumberOfProcessors.max(1)) as f64
        };
        let proc_100ns = unsafe {
            let mut c = FILETIME::default();
            let mut e = FILETIME::default();
            let mut k = FILETIME::default();
            let mut u = FILETIME::default();
            if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).is_ok() {
                filetime_to_100ns(k) + filetime_to_100ns(u)
            } else {
                0
            }
        };
        let (gpu_name, adapter3) = detect_primary_dxgi_adapter();
        let mut sampler = Self {
            num_cpus,
            prev_wall: Instant::now(),
            prev_proc_100ns: proc_100ns,
            gpu_name: gpu_name.clone(),
            adapter3,
            last_snapshot: TelemetrySnapshot {
                gpu_name,
                cpu_pct: 0.0,
                ram_mb: 0.0,
                gpu_mb: 0.0,
            },
        };
        sampler.sample_now();
        sampler
    }

    pub fn gpu_name(&self) -> &str {
        &self.gpu_name
    }

    pub fn poll(&mut self) -> TelemetrySnapshot {
        if self.prev_wall.elapsed().as_millis() >= 750 {
            self.sample_now();
        }
        self.last_snapshot.clone()
    }

    fn sample_now(&mut self) {
        let wall_100ns = self.prev_wall.elapsed().as_secs_f64() * 10_000_000.0;
        let proc_100ns = unsafe {
            let mut c = FILETIME::default();
            let mut e = FILETIME::default();
            let mut k = FILETIME::default();
            let mut u = FILETIME::default();
            if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).is_ok() {
                filetime_to_100ns(k) + filetime_to_100ns(u)
            } else {
                self.prev_proc_100ns
            }
        };

        let cpu_pct = if wall_100ns > 10_000.0 {
            let delta = proc_100ns.saturating_sub(self.prev_proc_100ns) as f64;
            ((delta / (wall_100ns * self.num_cpus)) * 100.0).clamp(0.0, 100.0) as f32
        } else {
            self.last_snapshot.cpu_pct
        };
        self.prev_wall = Instant::now();
        self.prev_proc_100ns = proc_100ns;

        let ram_mb = unsafe {
            let mut pmc = PROCESS_MEMORY_COUNTERS {
                cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            if K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut pmc,
                size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
            .as_bool()
            {
                (pmc.WorkingSetSize as f64 / (1024.0 * 1024.0)) as f32
            } else {
                0.0
            }
        };

        let gpu_mb = if let Some(a3) = self.adapter3.as_ref() {
            unsafe {
                let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                if a3
                    .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
                    .is_ok()
                {
                    (info.CurrentUsage as f64 / (1024.0 * 1024.0)) as f32
                } else {
                    0.0
                }
            }
        } else {
            0.0
        };

        self.last_snapshot = TelemetrySnapshot {
            gpu_name: self.gpu_name.clone(),
            cpu_pct,
            ram_mb,
            gpu_mb,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_sampler_reads_nonzero_ram() {
        let mut s = TelemetrySampler::new();
        let snap = s.poll();
        assert!(snap.ram_mb > 0.1, "expected RAM MB > 0.1, got {}", snap.ram_mb);
        assert!(!snap.gpu_name.is_empty());
    }
}
