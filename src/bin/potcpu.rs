#![cfg_attr(test, feature(proc_macro_hygiene))]

use anyhow::{anyhow, Result};
use itertools::Itertools;
use log::{info, trace, warn};
#[cfg(test)]
use mocktopus::macros::*;
use pot_rs::{get_running_pot_list, PotSystemConfig};
use std::collections::HashMap;
use std::process::{Command as PCommand, Stdio};
use structopt::StructOpt;
use structopt_flags::{LogLevel, QuietVerbose};

#[derive(Debug, StructOpt)]
#[structopt(name = "potcpu")]
struct Opt {
    #[structopt(flatten)]
    verbose: QuietVerbose,
    #[structopt(subcommand)]
    subcommand: Command,
}

#[derive(Debug, StructOpt)]
enum Command {
    /// Show the current CPU allocation
    #[structopt(name = "show")]
    Show,
    /// Get a cpu allocation for a new jail
    #[structopt(name = "get-cpu")]
    GetCpu(GetCpuOpt),
    /// Propose a new allocation layout if needed
    #[structopt(name = "rebalance")]
    Rebalance,
}

#[derive(Debug, StructOpt, Copy, Clone)]
struct GetCpuOpt {
    /// Amount of CPUs needed by that pot
    #[structopt(short = "n", long = "num", default_value = "1")]
    cpu_amount: u32,
}

type Allocation = Vec<u32>;
type AllocationRef = [u32];

fn allocation_from_utf8(v: &[u8]) -> Result<Allocation> {
    let output_string = std::str::from_utf8(v)?;
    let first_line = output_string
        .lines()
        .next()
        .ok_or_else(|| anyhow!("cpuset: no stdout"))?;
    let mask = first_line
        .split(':')
        .nth(1)
        .ok_or_else(|| anyhow!("cpuset: malformed stdout"))?;
    let result: Allocation = mask
        .split(',')
        .map(str::trim)
        .map(str::parse)
        .filter(std::result::Result::is_ok)
        .map(std::result::Result::unwrap)
        .collect();
    Ok(result)
}

fn allocation_to_string(allocation: &AllocationRef, ncpu: u32) -> String {
    if allocation.len() as u32 == ncpu {
        "not restricted".to_string()
    } else {
        let mut result = String::new();
        allocation.iter().for_each(|x| {
            result.push_str(&x.to_string());
            result.push(' ');
        });
        result
    }
}

#[cfg_attr(test, mockable)]
fn get_ncpu() -> Result<u32> {
    // real implementation
    let output = PCommand::new("/sbin/sysctl")
        .arg("-n")
        .arg("hw.ncpu")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;

    let output_string = std::str::from_utf8(&output.stdout)?;
    let ncpu: u32 = output_string.trim().parse()?;
    Ok(ncpu)
}

#[cfg_attr(test, mockable)]
fn get_cpusets(conf: &PotSystemConfig) -> Result<HashMap<String, Allocation>> {
    let mut result = HashMap::new();
    for pot in get_running_pot_list(conf) {
        let output = PCommand::new("/usr/bin/cpuset")
            .arg("-g")
            .arg("-j")
            .arg(&pot)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()?;
        if !output.status.success() {
            warn!("failed to get cpuset information for pot {}", pot);
            continue;
        }
        let allocation = allocation_from_utf8(&output.stdout)?;
        result.insert(pot, allocation);
    }
    Ok(result)
}

fn get_potcpuconstraints(
    allocations: &HashMap<String, Allocation>,
) -> Result<HashMap<String, u32>> {
    let mut result = HashMap::new();
    let ncpu = get_ncpu()?;
    for (pot_name, allocation) in allocations {
        if allocation.len() as u32 == ncpu {
            continue;
        }
        result.insert(pot_name.to_string(), allocation.len() as u32);
    }
    Ok(result)
}

fn show(opt: &Opt, conf: &PotSystemConfig) -> Result<()> {
    let ncpu = get_ncpu()?;
    let pot_cpusets = get_cpusets(conf)?;
    let pot_constraints = get_potcpuconstraints(&pot_cpusets)?;
    for (pot_name, allocation) in pot_cpusets {
        let constraint_string = match pot_constraints.iter().find(|(name, _)| *name == &pot_name) {
            Some(constraint) => constraint.1.to_string(),
            None => "NA".to_string(),
        };
        println!("pot {}:", pot_name);
        println!("\tCPU requested: {}", constraint_string);
        println!("\tCPU used: {}", allocation_to_string(&allocation, ncpu));
    }
    if opt.verbose.get_level_filter() > log::LevelFilter::Warn {
        let cpu_allocations = get_cpu_allocation(conf)?;
        for (cpu, pots) in cpu_allocations
            .into_iter()
            .sorted_by_key(|(cpu, _pots)| *cpu)
        {
            println!("CPU {} : allocated {} pots", cpu, pots);
        }
    }
    Ok(())
}

fn get_cpu_allocation(conf: &PotSystemConfig) -> Result<HashMap<u32, u32>> {
    let pot_cpusets = get_cpusets(conf)?;
    let ncpu = get_ncpu()?;
    let mut result: HashMap<u32, u32> = HashMap::new();
    for i in 0..ncpu {
        result.insert(i, 0);
    }
    for allocations in pot_cpusets.values() {
        for cpu_num in allocations {
            let old_value = result.remove(cpu_num).unwrap();
            result.insert(*cpu_num, old_value + 1);
        }
    }
    Ok(result)
}

fn get_cpu(_opt: &Opt, conf: &PotSystemConfig, cpu_amount: u32) -> Result<()> {
    let ncpu = get_ncpu()?;
    if ncpu <= cpu_amount {
        info!("Not enough CPU in the system to provide a meaningful allocation");
        return Ok(());
    }
    let cpu_allocations = get_cpu_allocation(conf)?;
    let sorted_cpu_allocations = cpu_allocations
        .iter()
        .sorted_by_key(|(cpu, _allocations)| *cpu)
        .sorted_by_key(|(_cpu, allocations)| *allocations);
    let mut cpu_string = String::new();
    for (cpu, _) in sorted_cpu_allocations.take(cpu_amount as usize) {
        cpu_string.push_str(&cpu.to_string());
        cpu_string.push(',');
    }
    println!("{}", cpu_string.trim_end_matches(','));
    Ok(())
}

fn rebalance(_opt: &Opt, conf: &PotSystemConfig) -> Result<()> {
    let cpu_counters = get_cpu_allocation(conf)?;
    let min = cpu_counters
        .iter()
        .min_by_key(|(_cpu, allocation)| *allocation)
        .unwrap();
    let max = cpu_counters
        .iter()
        .max_by_key(|(_cpu, allocation)| *allocation)
        .unwrap();
    if (max.1 - min.1) <= 1 {
        warn!("no need to rebalance");
        return Ok(());
    } else {
        info!("rebalance needed : min {} max {}", min.1, max.1);
    }
    let ncpu = get_ncpu()?;
    let pot_allocations = get_cpusets(conf)?;
    let pot_constraints = get_potcpuconstraints(&pot_allocations)?;
    let mut pot_new_allocations = HashMap::new();
    let mut cpu_index_counter: u32 = 0;
    for (pot_name, amount_cpu) in pot_constraints.iter().sorted_by(|a, b| a.0.cmp(b.0)) {
        let mut cpus: Vec<u32> = Vec::new();
        for _ in 0..*amount_cpu {
            cpus.push(cpu_index_counter);
            cpu_index_counter += 1;
            cpu_index_counter %= ncpu;
        }
        pot_new_allocations.insert(pot_name, cpus);
    }
    for (pot_name, pot_allocation) in pot_new_allocations {
        let mut cpuset_string = String::new();
        for cpu in pot_allocation {
            cpuset_string.push_str(&cpu.to_string());
            cpuset_string.push(',');
        }
        println!(
            "cpuset -l {} -j {}",
            cpuset_string.trim_end_matches(','),
            pot_name
        );
    }
    Ok(())
}
fn main() -> Result<()> {
    let opt = Opt::from_args();
    opt.verbose.set_log_level();
    trace!("potcpu start");

    let conf = PotSystemConfig::from_system()?;
    match opt.subcommand {
        Command::Show => show(&opt, &conf)?,
        Command::GetCpu(cmd_opt) => get_cpu(&opt, &conf, cmd_opt.cpu_amount)?,
        Command::Rebalance => rebalance(&opt, &conf)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocation_from_utf8() {
        let test_str =
            "jail 1 mask: 0, 1, 2, 3, 4, 5, 6, 7\njail 1 domain policy: first-touch mask: 0";
        let test_byte = test_str.as_bytes();
        let result = allocation_from_utf8(test_byte);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 8);
        assert_eq!(result[0], 0);
        assert_eq!(result[7], 7);

        let error1 = allocation_from_utf8("".as_bytes());
        assert!(error1.is_err());
        let error2 = allocation_from_utf8("no jail mask".as_bytes());
        assert!(error2.is_err());
    }

    #[test]
    fn test_allocation_to_string() {
        let uut = vec![0, 1];
        let result = allocation_to_string(&uut, 2);
        assert_eq!("not restricted".to_string(), result);
        let result = allocation_to_string(&uut, 8);
        assert_eq!("0 1".to_string(), result.trim());
    }

    #[test]
    fn test_get_potcpuconstraints() {
        use mocktopus::mocking::*;
        get_ncpu.mock_safe(|| MockResult::Return(Ok(2)));
        let empty_hm = HashMap::new();
        let result = get_potcpuconstraints(&empty_hm);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_empty());

        let mut one_full_allocation = HashMap::new();
        one_full_allocation.insert("pot-test".to_string(), vec![0, 1]);
        let result = get_potcpuconstraints(&one_full_allocation);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_empty());

        let mut one_half_allocation = HashMap::new();
        one_half_allocation.insert("pot-test".to_string(), vec![0]);
        let result = get_potcpuconstraints(&one_half_allocation);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_get_cpu_allocation() {
        use mocktopus::mocking::*;
        get_ncpu.mock_safe(|| MockResult::Return(Ok(4)));
        let mut pot_cpusets = HashMap::new();
        pot_cpusets.insert("pot0".to_string(), vec![0]);
        pot_cpusets.insert("pot12".to_string(), vec![1, 2]);
        pot_cpusets.insert("pot013".to_string(), vec![0, 1, 3]);
        get_cpusets.mock_safe(move |_| MockResult::Return(Ok(pot_cpusets.clone())));
        let conf = PotSystemConfig::default();

        let result = get_cpu_allocation(&conf);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.contains_key(&0));
        assert!(result.contains_key(&1));
        assert!(result.contains_key(&2));
        assert!(result.contains_key(&3));
        assert_eq!(result.get(&0).unwrap(), &2);
        assert_eq!(result.get(&1).unwrap(), &2);
        assert_eq!(result.get(&2).unwrap(), &1);
        assert_eq!(result.get(&3).unwrap(), &1);
    }
}
