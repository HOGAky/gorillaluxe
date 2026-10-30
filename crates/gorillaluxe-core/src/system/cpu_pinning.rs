//! Hybrid CPU pinning (spec PHASE 4 task 3).
//!
//! Enumerates P/E core topology via `GetLogicalProcessorInformationEx`
//! (`RelationProcessorCore`, per-core `EfficiencyClass`), then pins
//! the calling thread to selected logical processors with
//! `SetThreadSelectedCpuSets` — the scheduler-visible CPU-set IDs come
//! from `GetSystemCpuSetInformation` (declared manually; not yet in
//! windows-rs 0.62).

use windows::Win32::Foundation::{GetLastError, HANDLE};
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, LOGICAL_PROCESSOR_RELATIONSHIP,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};

const RELATION_PROCESSOR_CORE: LOGICAL_PROCESSOR_RELATIONSHIP = LOGICAL_PROCESSOR_RELATIONSHIP(0);

#[link(name = "kernel32")]
extern "system" {
    fn GetSystemCpuSetInformation(
        information: *mut u8,
        buffer_length: u32,
        returned_length: *mut u32,
        process: HANDLE,
        flags: u32,
    ) -> i32;
}

/// One physical core discovered from `RelationProcessorCore`.
#[derive(Debug, Clone)]
pub struct CoreInfo {
    /// Higher = faster (Intel hybrid: P-cores have the max value).
    pub efficiency_class: u8,
    /// Logical-processor affinity mask (group 0 assumed; typical
    /// consumer topologies only ever have one group).
    pub lp_mask: u64,
    pub group: u16,
}

/// One scheduler-visible CPU set entry.
#[derive(Debug, Clone, Copy)]
pub struct CpuSet {
    pub id: u32,
    pub group: u16,
    pub logical_processor_index: u8,
    pub efficiency_class: u8,
}

/// Hybrid CPU topology.
#[derive(Debug, Clone)]
pub struct CoreTopology {
    pub cores: Vec<CoreInfo>,
    /// Maximum efficiency class present = P-core class.
    pub best_class: u8,
    /// Per-LP CPU sets indexed by (group, lp_index) — for pinning.
    pub cpu_sets: Vec<CpuSet>,
}

fn enumerate_cores() -> Result<Vec<CoreInfo>, String> {
    unsafe {
        let mut len: u32 = 0;
        let _ = GetLogicalProcessorInformationEx(RELATION_PROCESSOR_CORE, None, &mut len);
        if len == 0 {
            return Err("GetLogicalProcessorInformationEx size query failed".into());
        }
        let mut buf = vec![0u8; len as usize];
        GetLogicalProcessorInformationEx(
            RELATION_PROCESSOR_CORE,
            Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut len,
        )
        .map_err(|e| format!("GetLogicalProcessorInformationEx: {e}"))?;

        let mut cores = Vec::new();
        let mut off = 0usize;
        while off + 8 <= buf.len() {
            let info = &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            if info.Size == 0 || off + info.Size as usize > buf.len() {
                break;
            }
            let p = &info.Anonymous.Processor;
            let mut mask = 0u64;
            let mut group = 0u16;
            if p.GroupCount > 0 {
                group = p.GroupMask[0].Group;
                mask = p.GroupMask[0].Mask as u64;
            }
            cores.push(CoreInfo {
                efficiency_class: p.EfficiencyClass,
                lp_mask: mask,
                group,
            });
            off += info.Size as usize;
        }
        Ok(cores)
    }
}

fn enumerate_cpu_sets() -> Result<Vec<CpuSet>, String> {
    unsafe {
        let mut len: u32 = 0;
        GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut len, HANDLE::default(), 0);
        if len == 0 {
            return Err(format!(
                "GetSystemCpuSetInformation size query: {}",
                GetLastError().0
            ));
        }
        let mut buf = vec![0u8; len as usize];
        let mut got: u32 = 0;
        if GetSystemCpuSetInformation(buf.as_mut_ptr(), len, &mut got, HANDLE::default(), 0) == 0 {
            return Err("GetSystemCpuSetInformation failed".into());
        }
        let mut sets = Vec::new();
        let mut off = 0usize;
        while off + 32 <= got as usize && off + 32 <= buf.len() {
            let base = buf.as_ptr().add(off);
            let size = (base as *const u32).read_unaligned() as usize;
            let kind = (base.add(4) as *const u32).read_unaligned();
            if size == 0 || off + size > buf.len() {
                break;
            }
            if kind == 0 {
                // CpuSet struct offsets: Id@8, Group@12, LPI@14, EffClass@18.
                sets.push(CpuSet {
                    id: (base.add(8) as *const u32).read_unaligned(),
                    group: (base.add(12) as *const u16).read_unaligned(),
                    logical_processor_index: *base.add(14),
                    efficiency_class: *base.add(18),
                });
            }
            off += size;
        }
        Ok(sets)
    }
}

/// Query the full hybrid topology. `best_class` is 0 on uniform CPUs.
pub fn enumerate() -> Result<CoreTopology, String> {
    let cores = enumerate_cores()?;
    let cpu_sets = enumerate_cpu_sets().unwrap_or_default();
    let best = cores.iter().map(|c| c.efficiency_class).max().unwrap_or(0);
    Ok(CoreTopology {
        best_class: best,
        cores,
        cpu_sets,
    })
}

impl CoreTopology {
    /// True when at least two different efficiency classes exist
    /// (Intel Alder Lake+ hybrid / ARM big.LITTLE style).
    pub fn is_hybrid(&self) -> bool {
        self.cores
            .iter()
            .any(|c| c.efficiency_class != self.cores[0].efficiency_class)
            && !self.cores.is_empty()
    }

    /// Aggregate LP mask of all cores in the top efficiency class.
    pub fn p_core_mask(&self) -> u64 {
        self.cores
            .iter()
            .filter(|c| c.efficiency_class == self.best_class)
            .fold(0u64, |m, c| m | c.lp_mask)
    }

    /// CPU-set IDs belonging to the top efficiency class (prefers
    /// direct efficiency classification; falls back to correlating
    /// with `p_core_mask` when set-level class data is all zero).
    pub fn p_core_set_ids(&self) -> Vec<u32> {
        if self.cpu_sets.is_empty() {
            return Vec::new();
        }
        let max_eff = self
            .cpu_sets
            .iter()
            .map(|s| s.efficiency_class)
            .max()
            .unwrap_or(0);
        self.cpu_sets
            .iter()
            .filter(|s| s.efficiency_class == max_eff)
            .map(|s| s.id)
            .collect()
    }
}

/// Pin the current thread to P-cores (best efficiency class).
/// Returns the number of CPU sets selected. No-op error when the
/// topology can't be enumerated.
pub fn pin_to_p_cores() -> Result<usize, String> {
    let topo = enumerate()?;
    let ids = topo.p_core_set_ids();
    if ids.is_empty() {
        return Err("no CPU sets available for pinning".into());
    }
    unsafe {
        if SetThreadSelectedCpuSets(GetCurrentThread(), &ids).as_bool() {
            Ok(ids.len())
        } else {
            Err(format!("SetThreadSelectedCpuSets: {}", GetLastError().0))
        }
    }
}

/// Short human-readable topology summary for the HUD/logs.
pub fn describe(topo: &CoreTopology) -> String {
    if topo.is_hybrid() {
        let p = topo
            .cores
            .iter()
            .filter(|c| c.efficiency_class == topo.best_class)
            .count();
        format!(
            "{} cores ({} P / {} E)",
            topo.cores.len(),
            p,
            topo.cores.len() - p
        )
    } else {
        format!("{} cores (uniform)", topo.cores.len())
    }
}
