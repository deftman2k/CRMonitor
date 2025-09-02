use chrono::Local;
use clap::Parser;
use local_ip_address::local_ip;
use serialport::SerialPort;
use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{CpuExt, NetworkExt, Networks, NetworksExt, System, SystemExt};

#[cfg(target_os = "windows")]
mod win_disk {
    use anyhow::Result;
    use serde::Deserialize;
    use wmi::{COMLibrary, WMIConnection};

    #[derive(Deserialize, Debug)]
    #[allow(non_snake_case)]
    pub struct PerfDisk {
        pub Name: String,
        pub DiskReadBytesPerSec: Option<u64>,
        pub DiskWriteBytesPerSec: Option<u64>,
    }

    pub struct DiskCounters {
        _com: COMLibrary,
        conn: WMIConnection,
    }

    impl DiskCounters {
        pub fn new() -> Result<Self> {
            let com = COMLibrary::new()?;
            let conn = WMIConnection::new(com.clone())?;
            Ok(Self { _com: com, conn })
        }
        pub fn read_total(&self) -> Result<(u64, u64)> {
            let q = "SELECT Name, DiskReadBytesPerSec, DiskWriteBytesPerSec \
                     FROM Win32_PerfFormattedData_PerfDisk_LogicalDisk";
            let rows: Vec<PerfDisk> = self.conn.raw_query(q)?;
            for row in rows {
                if row.Name == "_Total" {
                    return Ok((
                        row.DiskReadBytesPerSec.unwrap_or(0),
                        row.DiskWriteBytesPerSec.unwrap_or(0),
                    ));
                }
            }
            Ok((0, 0))
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about = "Send CPU/RAM/DSK_R/DSK_W/NET/IP over serial")]
struct Args {
    /// Serial port (ex: COM4, /dev/ttyUSB0),포트는 변경될수 있음.
    #[arg(short, long, default_value = "COM4")]
    port: String,
    /// Baud rate
    #[arg(short, long, default_value_t = 115200)]
    baud: u32,
    /// Interval seconds
    #[arg(short, long, default_value_t = 1.0)]
    interval: f64,
}

fn format_speed(bps: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bps >= GB {
        format!("{:.2} GB/s", bps / GB)
    } else if bps >= MB {
        format!("{:.2} MB/s", bps / MB)
    } else if bps >= KB {
        format!("{:.1} KB/s", bps / KB)
    } else {
        format!("{:.0} B/s", bps)
    }
}

fn get_primary_ip() -> String {
    local_ip().map(|ip| ip.to_string()).unwrap_or_else(|_| "0.0.0.0".into())
}

// sysinfo 0.29: 누적 네트워크 바이트 합(송+수)
fn total_net_bytes(networks: &Networks) -> u128 {
    networks
        .iter()
        .map(|(_, n)| n.total_received() as u128 + n.total_transmitted() as u128)
        .sum()
}

fn open_serial_blocking(port_name: &str, baud: u32) -> Box<dyn SerialPort> {
    loop {
        match serialport::new(port_name, baud).timeout(Duration::from_millis(1000)).open() {
            Ok(p) => {
                println!("[OK] Connected to {} @ {}", port_name, baud);
                thread::sleep(Duration::from_millis(1500));
                return p;
            }
            Err(e) => {
                eprintln!("[WAIT] Port '{}' not ready: {}", port_name, e);
                let ports = serialport::available_ports()
                    .map(|v| v.into_iter().map(|p| p.port_name).collect::<Vec<_>>())
                    .unwrap_or_default();
                eprintln!("       Available ports: {:?}", ports);
                thread::sleep(Duration::from_millis(1500));
            }
        }
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    println!(
        "[INFO] {}  send_stats (Rust/sysinfo 0.29)\n       port={} baud={} interval={}s",
        Local::now().format("%F %T"),
        args.port, args.baud, args.interval
    );

    let mut port = open_serial_blocking(&args.port, args.baud);

    let mut sys = System::new_all();
    sys.refresh_all();

    let mut last_net = total_net_bytes(sys.networks());
    let mut last_time = Instant::now();

    // Windows 전용 디스크 R/W 속도 소스(WMI)
    #[cfg(target_os = "windows")]
    let disk: Option<win_disk::DiskCounters> = match win_disk::DiskCounters::new() {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("[WARN] WMI init failed: {e}");
            None
        }
    };
    #[cfg(not(target_os = "windows"))]
    let disk: Option<()> = None;

    loop {
        sys.refresh_cpu();
        sys.refresh_memory();
        sys.refresh_networks();

        let cpu = {
            let cpus = sys.cpus();
            if cpus.is_empty() { 0 } else {
                let sum: f32 = cpus.iter().map(|c| c.cpu_usage()).sum();
                (sum / cpus.len() as f32).round() as i32
            }
        };

        let ram = {
            let total = sys.total_memory() as f64;
            let used = sys.used_memory() as f64;
            if total <= 0.0 { 0 } else { (used / total * 100.0).round() as i32 }
        };

        let elapsed = last_time.elapsed().as_secs_f64();

        // --- 디스크 R/W 속도 ---
        let (r_str, w_str) = {
            #[cfg(target_os = "windows")]
            {
                if let Some(ref d) = disk {
                    match d.read_total() {
                        Ok((r_bs, w_bs)) => (format_speed(r_bs as f64), format_speed(w_bs as f64)),
                        Err(e) => {
                            eprintln!("[WARN] WMI query failed: {e}");
                            ("0 B/s".to_string(), "0 B/s".to_string())
                        }
                    }
                } else {
                    ("0 B/s".to_string(), "0 B/s".to_string())
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                ("0 B/s".to_string(), "0 B/s".to_string())
            }
        };

        // --- 네트워크 속도 ---
        let now_net = total_net_bytes(sys.networks());
        let net_bps = if elapsed > 0.0 {
            (now_net.saturating_sub(last_net)) as f64 / elapsed
        } else { 0.0 };
        let net_str = format_speed(net_bps);
        last_net = now_net;

        last_time = Instant::now();

        // --- IP ---
        let ip = get_primary_ip();

        // 송신: cpu,ram,disk_r,disk_w,net,ip
        let line = format!("{},{},{},{},{},{}\n", cpu, ram, r_str, w_str, net_str, ip);
        if let Err(e) = port.write_all(line.as_bytes()) {
            eprintln!("\n[WARN] Serial write failed: {}. Reconnecting...", e);
            port = open_serial_blocking(&args.port, args.baud);
            continue;
        }

        print!(
            "\rCPU:{:3}%  RAM:{:3}%  DSK R:{} W:{}  NET:{:>10}  IP:{}      ",
            cpu, ram, r_str, w_str, net_str, ip
        );
        io::stdout().flush().ok();

        thread::sleep(Duration::from_secs_f64(args.interval));
    }
}
