#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::{mpsc, Arc, Mutex};
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use image::{ImageBuffer, Rgba};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use sysinfo::System;

#[cfg(windows)]
mod win_disk_wmi {
    use serde::Deserialize;
    use wmi::{COMLibrary, WMIConnection, WMIError};

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
        pub fn new() -> Result<Self, WMIError> {
            let com = COMLibrary::new()?;
            let conn = WMIConnection::new(com.clone())?;
            Ok(Self { _com: com, conn })
        }

        pub fn read_total(&self) -> Result<(u64, u64), WMIError> {
            // Use PhysicalDisk to include all physical devices; aggregate _Total
            let q = "SELECT Name, DiskReadBytesPerSec, DiskWriteBytesPerSec \
                     FROM Win32_PerfFormattedData_PerfDisk_PhysicalDisk";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    CpuRam,
    DiskNet,
}

#[derive(Debug)]
enum AppEvent {
    Update { tooltip: String, rgba: Vec<u8>, w: u32, h: u32 },
}

#[derive(Debug)]
enum MonitorCmd {
    ResetDiffs,
}

#[derive(Debug)]
enum OverlayCmd {
    Close,
}

#[derive(Default, Clone, Copy)]
struct RollingMax {
    val: f64,
}
impl RollingMax {
    fn update(&mut self, current: f64) -> f64 {
        if current > self.val {
            self.val = current;
        }
        self.val *= 0.985;
        if self.val < current {
            self.val = current;
        }
        if self.val <= 1e-6 {
            self.val = 1.0;
        }
        self.val
    }
}

#[derive(Default, Clone, Copy)]
struct NetBytes {
    rx: u64,
    tx: u64,
}
#[cfg(not(windows))]
#[derive(Default, Clone, Copy)]
struct DiskBytes {
    read: u64,
    write: u64,
}

#[derive(Debug, Clone, Default)]
struct Metrics {
    cpu_pct: f32,
    ram_pct: f32,
    disk_r_mbps: f64,
    disk_w_mbps: f64,
    disk_mbps: f64,
    net_kbps: f64,
    disk_pct: f64,
    net_pct: f64,
    net_info: String,
}

#[derive(Clone, Copy)]
struct WindowPosition {
    x: i32,
    y: i32,
}

fn main() {
    let event_loop: EventLoop<AppEvent> = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let mode = Arc::new(Mutex::new(Mode::CpuRam));
    let shared_metrics: Arc<Mutex<Metrics>> = Arc::new(Mutex::new(Metrics::default()));
    let overlay_pos = Arc::new(Mutex::new(WindowPosition { x: 8, y: 8 }));

    let menu = Menu::new();
    let about_text = format!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let about_item = MenuItem::new(&about_text, false, None);
    let switch_item = MenuItem::new("Switch to Disk/Network", true, None);
    let overlay_item = MenuItem::new("Hide Overlay", true, None);
    // Serial menu items
    let mut serial_enabled = false;
    let default_port = std::env::var("CRTM_SERIAL_PORT").unwrap_or_else(|_| "COM4".to_string());
    let serial_toggle_item = MenuItem::new("[ ] Serial Output", true, None);
    let serial_port_label = MenuItem::new(&format!("Port: {}", default_port), false, None);
    let serial_refresh_item = MenuItem::new("Refresh Ports", true, None);
    let quit_item = PredefinedMenuItem::quit(None);
    menu.append(&about_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&switch_item).unwrap();
    menu.append(&overlay_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&serial_toggle_item).unwrap();
    menu.append(&serial_port_label).unwrap();
    menu.append(&serial_refresh_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&quit_item).unwrap();

    let (w, h) = icon_size();
    let rgba = vec![0u8; (w * h * 4) as usize];
    let icon = Icon::from_rgba(rgba, w, h).expect("icon");
    let mut tray: Option<TrayIcon> = Some(
        TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("Starting...")
            .with_icon(icon)
            .build()
            .expect("tray"),
    );

    let (cmd_tx, cmd_rx) = mpsc::channel::<MonitorCmd>();
    let mut overlay_tx_opt = Some(spawn_overlay_window(
        Arc::clone(&shared_metrics),
        Arc::clone(&overlay_pos),
    ));

    let serial_cfg = Arc::new(Mutex::new(SerialCfg { enabled: false, port: default_port.clone() }));

    spawn_monitor_thread(
        proxy.clone(),
        mode.clone(),
        w,
        h,
        cmd_rx,
        Arc::clone(&shared_metrics),
    );

    spawn_serial_worker(Arc::clone(&shared_metrics), Arc::clone(&serial_cfg));

    let menu_rx = MenuEvent::receiver();

    // dynamic port items; built on first loop
    let mut port_items: Vec<(MenuItem, String)> = Vec::new();
    let mut ports_built = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {} //
            Event::UserEvent(AppEvent::Update { tooltip, rgba, w, h }) => {
                if let Some(tray_icon) = tray.as_mut() {
                    if let Some(ic) = Icon::from_rgba(rgba, w, h).ok() {
                        let _ = tray_icon.set_icon(Some(ic));
                    }
                    let _ = tray_icon.set_tooltip(Some(&tooltip));
                }
            }
            Event::MainEventsCleared => {
                if !ports_built {
                    if let Ok(ports) = serialport::available_ports() {
                        for p in ports {
                            let label = format!("Use {}", p.port_name);
                            let item = MenuItem::new(&label, true, None);
                            menu.append(&item).ok();
                            port_items.push((item, p.port_name));
                        }
                    }
                    ports_built = true;
                }

                while let Ok(ev) = menu_rx.try_recv() {
                    if ev.id == switch_item.id() {
                        let mut m = mode.lock().unwrap();
                        *m = match *m {
                            Mode::CpuRam => Mode::DiskNet,
                            Mode::DiskNet => Mode::CpuRam,
                        };
                        let _ = switch_item.set_text(match *m {
                            Mode::CpuRam => "Switch to Disk/Network",
                            Mode::DiskNet => "Switch to CPU/RAM",
                        });
                        if let Mode::DiskNet = *m {
                            let _ = cmd_tx.send(MonitorCmd::ResetDiffs);
                        }
                    } else if ev.id == overlay_item.id() {
                        if let Some(tx) = overlay_tx_opt.take() {
                            let _ = tx.send(OverlayCmd::Close);
                            let _ = overlay_item.set_text("Show Overlay");
                        } else {
                            overlay_tx_opt = Some(spawn_overlay_window(
                                Arc::clone(&shared_metrics),
                                Arc::clone(&overlay_pos),
                            ));
                            let _ = overlay_item.set_text("Hide Overlay");
                        }
                    } else if ev.id == serial_toggle_item.id() {
                        serial_enabled = !serial_enabled;
                        let _ = serial_toggle_item.set_text(if serial_enabled { "[x] Serial Output" } else { "[ ] Serial Output" });
                        let mut sc = serial_cfg.lock().unwrap();
                        sc.enabled = serial_enabled;
                    } else if ev.id == serial_refresh_item.id() {
                        if let Ok(ports) = serialport::available_ports() {
                            for p in ports {
                                if !port_items.iter().any(|(_, name)| name == &p.port_name) {
                                    let label = format!("Use {}", p.port_name);
                                    let item = MenuItem::new(&label, true, None);
                                    menu.append(&item).ok();
                                    port_items.push((item, p.port_name));
                                }
                            }
                        }
                    } else if ev.id == quit_item.id() {
                        if let Some(tx) = overlay_tx_opt.take() { let _ = tx.send(OverlayCmd::Close); }
                        *control_flow = ControlFlow::Exit;
                    } else {
                        // Port selection
                        for (it, name) in &port_items {
                            if ev.id == it.id() {
                                let mut sc = serial_cfg.lock().unwrap();
                                sc.port = name.clone();
                                let _ = serial_port_label.set_text(&format!("Port: {}", sc.port));
                                break;
                            }
                        }
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                *control_flow = ControlFlow::Exit;
            }
            _ => {} //
        }
    });
}
fn icon_size() -> (u32, u32) {
    if let Ok(s) = std::env::var("CRTM_ICON_SIZE") {
        if let Ok(n) = s.parse::<u32>() { return (n, n); }
    }
    (24, 24)
}

fn spawn_monitor_thread(
    proxy: EventLoopProxy<AppEvent>,
    mode: Arc<Mutex<Mode>>,
    w: u32,
    h: u32,
    cmd_rx: mpsc::Receiver<MonitorCmd>,
    shared_metrics: Arc<Mutex<Metrics>>,
) {
    thread::spawn(move || {
        let mut sys = System::new();
        sys.refresh_cpu();
        sys.refresh_memory();

        let (mut active_if_index, mut net_info_cache) = get_network_profile_info();
        let mut prev_net = snapshot_network(active_if_index);
        // On Windows, use WMI to avoid admin privileges for disk stats.
        #[cfg(windows)]
        let disk_counters = win_disk_wmi::DiskCounters::new().ok();
        #[cfg(not(windows))]
        let mut prev_disk = snapshot_disk();
        let mut last = Instant::now();
        let mut last_net_info_check = Instant::now();

        let mut max_disk_mbps = RollingMax::default();
        let mut max_net_kbps = RollingMax::default();

        loop {
            if cmd_rx.try_recv().is_ok() {
                prev_net = snapshot_network(active_if_index);
                #[cfg(not(windows))]
                {
                    prev_disk = snapshot_disk();
                }
                max_disk_mbps = RollingMax::default();
                max_net_kbps = RollingMax::default();
                last = Instant::now();
            }

            let now = Instant::now();
            let dt = (now - last).as_secs_f64();
            last = now;

            sys.refresh_cpu();
            sys.refresh_memory();

            let cpu = sys.global_cpu_info().cpu_usage() as f32;
            let mem_pct = if sys.total_memory() > 0 {
                (sys.used_memory() as f32 / sys.total_memory() as f32) * 100.0
            } else { 0.0 };

            let cur_net = snapshot_network(active_if_index);
            let (mut net_kbps, prev_net_out) = diff_network(prev_net, cur_net, dt.max(1e-3));
            prev_net = prev_net_out;

            // Disk throughput (MB/s)
            #[cfg(windows)]
            let (mut disk_r_mbps, mut disk_w_mbps, mut disk_mbps): (f64, f64, f64) = {
                if let Some(ref dc) = disk_counters {
                    if let Ok((r_bs, w_bs)) = dc.read_total() {
                        let r = r_bs as f64 / (1024.0 * 1024.0);
                        let w = w_bs as f64 / (1024.0 * 1024.0);
                        (r, w, r + w)
                    } else { (0.0, 0.0, 0.0) }
                } else { (0.0, 0.0, 0.0) }
            };
            #[cfg(not(windows))]
            let (mut disk_r_mbps, mut disk_w_mbps, mut disk_mbps, prev_disk_out) = {
                let cur_disk = snapshot_disk();
                diff_disk(prev_disk, cur_disk, dt.max(1e-3))
            };
            #[cfg(not(windows))]
            {
                prev_disk = prev_disk_out;
            }

            if disk_mbps.is_finite() && disk_mbps < 0.001 { disk_mbps = 0.0; }
            if disk_r_mbps.is_finite() && disk_r_mbps < 0.001 { disk_r_mbps = 0.0; }
            if disk_w_mbps.is_finite() && disk_w_mbps < 0.001 { disk_w_mbps = 0.0; }
            if net_kbps.is_finite()  && net_kbps  < 0.1   { net_kbps  = 0.0; }

            let disk_floor = 0.25;
            let net_floor  = 4.0;
            let disk_scale = max_disk_mbps.update(disk_mbps.max(disk_floor));
            let net_scale  = max_net_kbps.update(net_kbps .max(net_floor));
            let disk_pct = ((disk_mbps / disk_scale) * 100.0).clamp(0.0, 100.0);
            let net_pct  = ((net_kbps  / net_scale ) * 100.0).clamp(0.0, 100.0);

            if now.duration_since(last_net_info_check) > Duration::from_secs(60) {
                let (new_index, new_info) = get_network_profile_info();
                if new_index != active_if_index {
                    active_if_index = new_index;
                    prev_net = snapshot_network(active_if_index); // Reset counter on interface change
                }
                net_info_cache = new_info;
                last_net_info_check = now;
            }

            {
                let mut sm = shared_metrics.lock().unwrap();
                sm.cpu_pct   = cpu;
                sm.ram_pct   = mem_pct;
                sm.disk_r_mbps = disk_r_mbps;
                sm.disk_w_mbps = disk_w_mbps;
                sm.disk_mbps = disk_mbps;
                sm.net_kbps  = net_kbps;
                sm.disk_pct  = disk_pct;
                sm.net_pct   = net_pct;
                sm.net_info  = net_info_cache.clone();
            }

            let current_mode = *mode.lock().unwrap();
            match current_mode {
                Mode::CpuRam => {
                    let tooltip = format!("CPU: {:.1}% | RAM: {:.1}%", cpu, mem_pct);
                    let rgba = make_icon_bars(w, h, cpu as f64, mem_pct as f64, Palette::CpuRam);
                    let _ = proxy.send_event(AppEvent::Update { tooltip, rgba, w, h });
                }
                Mode::DiskNet => {
                    let tooltip = format!(
                        "Disk R/W: {:.2}/{:.2} MB/s ({:.0}%) | Net: {} ({:.0}%)",
                        disk_r_mbps, disk_w_mbps, disk_pct, format_speed_auto(net_kbps), net_pct
                    );
                    let rgba = make_icon_bars(w, h, disk_pct, net_pct, Palette::DiskNet);
                    let _ = proxy.send_event(AppEvent::Update { tooltip, rgba, w, h });
                }
            }

            thread::sleep(Duration::from_millis(1000));
        }
    });
}

#[cfg(windows)]
#[inline]
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

#[cfg(windows)]
fn spawn_overlay_window(
    shared: Arc<Mutex<Metrics>>,
    position: Arc<Mutex<WindowPosition>>,
) -> mpsc::Sender<OverlayCmd> {
    use std::mem::zeroed;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Graphics::Gdi::{*};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{*};

    let (tx, rx) = mpsc::channel::<OverlayCmd>();

    thread::spawn(move || unsafe {
        let class_name: Vec<u16> = "CRMonitorOverlay\0".encode_utf16().collect();
        let h_instance = GetModuleHandleW(null_mut());

        extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            unsafe {
                match msg {
                    WM_NCHITTEST => {
                        let x = (l as u32 & 0xFFFF) as i16 as i32;
                        let y = (l as i32) >> 16;
                        let point = POINT { x, y };

                        let mut window_rect = zeroed();
                        GetWindowRect(hwnd, &mut window_rect);

                        let handle_rect = RECT {
                            left: window_rect.left,
                            top: window_rect.top,
                            right: window_rect.left + 16,
                            bottom: window_rect.top + 16,
                        };

                        if PtInRect(&handle_rect, point) != 0 {
                            return HTCAPTION as isize;
                        }
                        return HTTRANSPARENT as isize;
                    }
                    WM_PAINT => {
                        let mut ps: PAINTSTRUCT = zeroed();
                        let hdc = BeginPaint(hwnd, &mut ps);

                        const COLKEY: u32 = rgb(255, 0, 255);
                        let rect = RECT { left: 0, top: 0, right: 300, bottom: 160 };
                        let brush = CreateSolidBrush(COLKEY);
                        FillRect(hdc, &rect, brush);
                        DeleteObject(brush as _);

                        let handle_rect = RECT { left: 0, top: 0, right: 16, bottom: 16 };
                        let handle_brush = CreateSolidBrush(rgb(100, 100, 100));
                        FillRect(hdc, &handle_rect, handle_brush);
                        DeleteObject(handle_brush as _);

                        SetBkMode(hdc, TRANSPARENT as i32);
                        SetTextColor(hdc, rgb(200, 200, 200));

                        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Metrics;
                        if !ptr.is_null() {
                            let m = &*ptr;
                            let mut y = 6;
                            draw_text_shadowed(hdc, 8, y, "--System Usage--");
                            y += 20;
                            draw_text_shadowed(hdc, 8, y, &format!("CPU: {:.1}%", m.cpu_pct));
                            y += 18;
                            draw_text_shadowed(hdc, 8, y, &format!("RAM: {:.1}%", m.ram_pct));
                            y += 18;
                            draw_text_shadowed(hdc, 8, y, &format!("DISK R: {:.2} MB/s", m.disk_r_mbps));
                            y += 18;
                            draw_text_shadowed(hdc, 8, y, &format!("DISK W: {:.2} MB/s", m.disk_w_mbps));
                            y += 18;
                            draw_text_shadowed(hdc, 8, y, &format!("DISK TOT: {:.2} MB/s ({:.0} %)", m.disk_mbps, m.disk_pct));
                            y += 18;
                            draw_text_shadowed(
                                hdc,
                                8,
                                y,
                                &format!("NET: {} ({:.0} %)", format_speed_auto(m.net_kbps), m.net_pct),
                            );
                            y += 20;
                            draw_text_shadowed(hdc, 8, y, &m.net_info);
                        }

                        EndPaint(hwnd, &ps);
                        return 0;
                    }
                    WM_CLOSE => {
                        DestroyWindow(hwnd);
                        return 0;
                    }
                    WM_DESTROY => {
                        PostQuitMessage(0);
                        return 0;
                    }
                    _ => {} //
                }
                DefWindowProcW(hwnd, msg, w, l)
            }
        }

        unsafe fn draw_text_shadowed(
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            x: i32,
            y: i32,
            s: &str,
        ) {
            use windows_sys::Win32::Graphics::Gdi::{SetTextColor, TextOutW};

            let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();

            let prev = SetTextColor(hdc, rgb(255, 255, 255));
            TextOutW(hdc, x, y, w.as_ptr(), (w.len() - 1) as i32);
            SetTextColor(hdc, prev);
        }


        let mut wc: WNDCLASSW = zeroed();
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = h_instance;
        wc.lpszClassName = class_name.as_ptr();
        wc.hCursor = LoadCursorW(0, IDC_ARROW);

        if RegisterClassW(&wc) == 0 {} // NOTE: This can fail if the class is already registered, which is fine.

        let initial_pos = *position.lock().unwrap();
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED, // WS_EX_TOPMOST 제거하여 다른 창에 가려질 수 있도록 함
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_POPUP,
            initial_pos.x,
            initial_pos.y,
            300,
            160, // Increased height
            0,
            0,
            h_instance,
            null_mut(),
        );
        if hwnd == 0 {
            return;
        }

        // 전체 윈도우 알파 (0~255)
        const COLKEY: u32 = rgb(255, 0, 255); // 마젠타를 투명색으로 사용
        SetLayeredWindowAttributes(hwnd, COLKEY, 0, LWA_COLORKEY); // ★ 이 색은 완전 투명
        ShowWindow(hwnd, SW_SHOW);

        // Metrics 포인터 저장 (Clone 필요)
        let mut boxed_metrics = Box::new(shared.lock().unwrap().clone());
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&*boxed_metrics as *const Metrics) as isize);

        // 메시지 루프
        let mut msg: MSG = zeroed();
        loop {
            // 외부 종료 명령
            if let Ok(OverlayCmd::Close) = rx.try_recv() {
                // 현재 위치 저장
                let mut rect: RECT = zeroed();
                if GetWindowRect(hwnd, &mut rect) != 0 {
                    let mut pos = position.lock().unwrap();
                    pos.x = rect.left;
                    pos.y = rect.top;
                }
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }

            // 최신 메트릭 반영 + 다시 그리기 (Clone 필요)
            *boxed_metrics = shared.lock().unwrap().clone();
            InvalidateRect(hwnd, std::ptr::null(), 1);

            while PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    return;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    tx
}

#[cfg(not(windows))]
fn spawn_overlay_window(
    _shared: Arc<Mutex<Metrics>>,
    _position: Arc<Mutex<WindowPosition>>,
) -> mpsc::Sender<OverlayCmd> {
    let (tx, _rx) = mpsc::channel::<OverlayCmd>();
    tx
}

#[cfg(windows)]
fn snapshot_network(interface_index: u32) -> NetBytes {
    use std::ptr::null_mut;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2
    };

    if interface_index == 0 {
        return NetBytes::default();
    }

    unsafe {
        let mut table_ptr: *mut MIB_IF_TABLE2 = null_mut();
        if GetIfTable2(&mut table_ptr) != 0 || table_ptr.is_null() {
            return NetBytes::default();
        }
        let table = &*table_ptr;
        let first: *const MIB_IF_ROW2 = table.Table.as_ptr();

        for i in 0..(table.NumEntries as usize) {
            let row = &*first.add(i);
            if row.InterfaceIndex == interface_index {
                let bytes = NetBytes { rx: row.InOctets, tx: row.OutOctets };
                FreeMibTable(table_ptr as *mut _);
                return bytes;
            }
        }
        FreeMibTable(table_ptr as *mut _);
    }
    NetBytes::default()
}

#[cfg(not(windows))]
fn snapshot_network(_interface_index: u32) -> NetBytes {
    NetBytes::default()
}


// Removed old Windows IOCTL-based disk snapshot: replaced by WMI path

#[cfg(not(windows))]
fn snapshot_disk() -> DiskBytes {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "linux")] {
            let mut read_bytes: u64 = 0;
            let mut write_bytes: u64 = 0;
            if let Ok(ds) = procfs::diskstats() {
                for d in ds {
                    let sec_sz = 512u64;
                    read_bytes  = read_bytes.saturating_add(d.read_sectors.unwrap_or(0)  as u64 * sec_sz);
                    write_bytes = write_bytes.saturating_add(d.write_sectors.unwrap_or(0) as u64 * sec_sz);
                }
            }
            DiskBytes { read: read_bytes, write: write_bytes }
        } else {
            DiskBytes { read: 0, write: 0 }
        }
    }
}

fn diff_network(prev: NetBytes, cur: NetBytes, dt: f64) -> (f64, NetBytes) {
    let drx = cur.rx.saturating_sub(prev.rx);
    let dtx = cur.tx.saturating_sub(prev.tx);
    let kbps = if dt > 0.0 { (drx as f64 + dtx as f64) / dt / 1024.0 } else { 0.0 };
    (kbps, cur)
}

#[cfg(not(windows))]
fn diff_disk(prev: DiskBytes, cur: DiskBytes, dt: f64) -> (f64, f64, f64, DiskBytes) {
    let dread  = cur.read .saturating_sub(prev.read);
    let dwrite = cur.write.saturating_sub(prev.write);
    let r_mbps = if dt > 0.0 { (dread  as f64) / dt / (1024.0 * 1024.0) } else { 0.0 };
    let w_mbps = if dt > 0.0 { (dwrite as f64) / dt / (1024.0 * 1024.0) } else { 0.0 };
    let mbps   = r_mbps + w_mbps;
    (r_mbps, w_mbps, mbps, cur)
}

#[derive(Clone, Copy)]
enum Palette { CpuRam, DiskNet }

fn make_icon_bars(w: u32, h: u32, v1_pct: f64, v2_pct: f64, palette: Palette) -> Vec<u8> {
    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, Rgba([0, 0, 0, 0]));

    let margin = 2u32;
    let inner_h_f = (h.saturating_sub(margin * 2)) as f64;

    let mut bar_w = w.saturating_sub(margin * 3) / 2;
    if bar_w == 0 { bar_w = 1; }

    let left_x = margin;
    let right_x = margin * 2 + bar_w;

    let clamp_pct = |p: f64| -> f64 {
        if p.is_finite() { p.clamp(0.0, 100.0) } else { 0.0 }
    };
    let v1p = clamp_pct(v1_pct);
    let v2p = clamp_pct(v2_pct);

    let v_to_h = |p: f64| -> u32 {
        if inner_h_f <= 0.0 { return 0; }
        if p >= 99.5 { inner_h_f.round() as u32 }
        else if p <= 0.0 { 0 }
        else { ((inner_h_f * (p / 100.0)).round() as u32).max(1) }
    };
    let v1_h = v_to_h(v1p);
    let v2_h = v_to_h(v2p);

    let base_y = h.saturating_sub(margin);

    let c1 = match palette { Palette::CpuRam => color_ramp(v1p), Palette::DiskNet => color_ramp_disk(v1p) };
    let c2 = match palette { Palette::CpuRam => color_ramp(v2p), Palette::DiskNet => color_ramp_disk(v2p) };

    draw_bar(&mut img, left_x,  base_y, bar_w, v1_h, c1);
    draw_bar(&mut img, right_x, base_y, bar_w, v2_h, c2);

    img.into_raw()
}

fn draw_bar(
    img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    x: u32,
    base_y: u32,
    w: u32,
    h: u32,
    color: [u8; 4],
) {
    if w == 0 || base_y == 0 { return; }
    let y0 = base_y.saturating_sub(h);
    let x1 = (x + w).min(img.width());
    let y1 = base_y.min(img.height());
    for yy in y0..y1 {
        for xx in x..x1 {
            let px = img.get_pixel_mut(xx, yy);
            *px = Rgba(color);
        }
    }
}

fn color_ramp(pct: f64) -> [u8; 4] {
    let t = (pct / 100.0).clamp(0.0, 1.0) as f32;
    if t < 0.5 {
        lerp_rgb([0x29, 0xBF, 0x12], [0xF2, 0xCC, 0x0C], t * 2.0)
    } else {
        lerp_rgb([0xF2, 0xCC, 0x0C], [0xEF, 0x23, 0x3C], (t - 0.5) * 2.0)
    }
}

fn color_ramp_disk(pct: f64) -> [u8; 4] {
    let t = (pct / 100.0).clamp(0.0, 1.0) as f32;
    if t < 0.5 {
        lerp_rgb([0x24, 0x7B, 0xFF], [0x73, 0x3A, 0xE6], t * 2.0)
    } else {
        lerp_rgb([0x73, 0x3A, 0xE6], [0xE6, 0x1E, 0x9A], (t - 0.5) * 2.0)
    }
}

fn lerp_rgb(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 4] {
    let mix = |aa: u8, bb: u8| -> u8 { ((aa as f32) + (bb as f32 - aa as f32) * t).round().clamp(0.0, 255.0) as u8 };
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]), 0xFF]
}

fn format_speed_auto(kbps: f64) -> String {
    if kbps < 1024.0 {
        format!("{:.0} KB/s", kbps)
    } else if kbps < 1024.0 * 1024.0 {
        format!("{:.1} MB/s", kbps / 1024.0)
    } else {
        format!("{:.1} GB/s", kbps / (1024.0 * 1024.0))
    }
}

#[derive(Clone)]
struct SerialCfg { enabled: bool, port: String }

fn spawn_serial_worker(shared: Arc<Mutex<Metrics>>, cfg: Arc<Mutex<SerialCfg>>) {
    thread::spawn(move || {
        let mut port: Option<Box<dyn serialport::SerialPort>> = None;
        loop {
            let (enabled, port_name, snapshot) = {
                let c = cfg.lock().unwrap();
                let s = shared.lock().unwrap().clone();
                (c.enabled, c.port.clone(), s)
            };

            if enabled {
                // ensure port open
                if port.as_ref().map(|p| p.name().unwrap_or_default()) != Some(port_name.clone()) {
                    port = None;
                }
                if port.is_none() {
                    match serialport::new(&port_name, 115200).timeout(Duration::from_millis(1000)).open() {
                        Ok(p) => port = Some(p),
                        Err(_) => { /* keep trying next loop */ }
                    }
                }
                if let Some(p) = port.as_mut() {
                    let cpu = snapshot.cpu_pct.round() as i32;
                    let ram = snapshot.ram_pct.round() as i32;
                    let disk_r = format_speed_bytes(snapshot.disk_r_mbps * 1024.0 * 1024.0);
                    let disk_w = format_speed_bytes(snapshot.disk_w_mbps * 1024.0 * 1024.0);
                    let net_bps = snapshot.net_kbps * 1024.0;
                    let net_s = format_speed_bytes(net_bps);
                    let ip = get_primary_ip();
                    let line = format!("{},{},{},{},{},{}\n", cpu, ram, disk_r, disk_w, net_s, ip);
                    let _ = p.write_all(line.as_bytes());
                }
            } else {
                port = None;
            }
            thread::sleep(Duration::from_millis(1000));
        }
    });
}

fn format_speed_bytes(bps: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bps >= GB { format!("{:.2} GB/s", bps / GB) }
    else if bps >= MB { format!("{:.2} MB/s", bps / MB) }
    else if bps >= KB { format!("{:.1} KB/s", bps / KB) }
    else { format!("{:.0} B/s", bps) }
}

fn get_primary_ip() -> String {
    local_ip_address::local_ip().map(|ip| ip.to_string()).unwrap_or_else(|_| "0.0.0.0".into())
}

#[cfg(windows)]
fn get_network_profile_info() -> (u32, String) {
    use std::process::Command;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let ps_cmd = "Get-NetConnectionProfile | Where-Object {$_.IPv4Connectivity -ne 'NoTraffic'} | ForEach-Object { $ip = Get-NetIPAddress -InterfaceIndex $_.InterfaceIndex -AddressFamily IPv4 | Select-Object -First 1; $_.InterfaceIndex.ToString() + ';' + $_.Name + ' (' + $_.NetworkCategory + '): ' + $ip.IPAddress } | Select-Object -First 1";
    let output = Command::new("powershell")
        .args(&["-NoProfile", "-Command", ps_cmd])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Some((if_index_str, display_str)) = stdout.split_once(';') {
                if let Ok(if_index) = if_index_str.trim().parse::<u32>() {
                    return (if_index, display_str.trim().to_string());
                }
            }
        }
    }
    (0, "Network info not available".to_string())
}

#[cfg(not(windows))]
fn get_network_profile_info() -> (u32, String) {
    (0, "Network info not available on this OS".to_string())
}
