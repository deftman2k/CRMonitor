#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use image::{ImageBuffer, Rgba};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use sysinfo::{Networks, System};

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
    ResetDiffs, // Disk/Net 모드 전환 직후 prev 재설정
}

#[derive(Debug)]
enum OverlayCmd {
    Close, // 오버레이 창 닫기(토글 Off)
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
#[derive(Default, Clone, Copy)]
struct DiskBytes {
    read: u64,
    write: u64,
}

// 오버레이에 표시할 공유 메트릭
#[derive(Debug, Clone, Copy)]
struct Metrics {
    cpu_pct: f32,
    ram_pct: f32,
    disk_mbps: f64,
    net_kbps: f64,
    disk_pct: f64,
    net_pct: f64,
}
impl Default for Metrics {
    fn default() -> Self {
        Self { cpu_pct: 0.0, ram_pct: 0.0, disk_mbps: 0.0, net_kbps: 0.0, disk_pct: 0.0, net_pct: 0.0 }
    }
}

#[derive(Clone, Copy)]
struct WindowPosition {
    x: i32,
    y: i32,
}

fn main() {
    // 이벤트 루프 (유저 이벤트 허용)
    let event_loop: EventLoop<AppEvent> = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let mode = Arc::new(Mutex::new(Mode::CpuRam));
    let shared_metrics: Arc<Mutex<Metrics>> = Arc::new(Mutex::new(Metrics::default()));
    let overlay_pos = Arc::new(Mutex::new(WindowPosition { x: 8, y: 8 }));

    // 트레이 메뉴
    let menu = Menu::new();
    let about_text = format!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let about_item = MenuItem::new(&about_text, false, None);
    let switch_item = MenuItem::new("Switch to Disk/Network", true, None);
    let overlay_item = MenuItem::new("Hide Overlay", true, None);
    let quit_item = PredefinedMenuItem::quit(None);
    menu.append(&about_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&switch_item).unwrap();
    menu.append(&overlay_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&quit_item).unwrap();

    // 트레이 아이콘 최초 설정
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

    // 모니터 스레드와 오버레이 제어 채널
    let (cmd_tx, cmd_rx) = mpsc::channel::<MonitorCmd>();
    let mut overlay_tx_opt = Some(spawn_overlay_window(
        Arc::clone(&shared_metrics),
        Arc::clone(&overlay_pos),
    ));

    // 모니터 스레드 시작
    spawn_monitor_thread(
        proxy.clone(),
        mode.clone(),
        w,
        h,
        cmd_rx,
        Arc::clone(&shared_metrics),
    );

    let menu_rx = MenuEvent::receiver();

    // 이벤트 루프
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
                    } else if ev.id == quit_item.id() {
                        if let Some(tx) = overlay_tx_opt.take() { let _ = tx.send(OverlayCmd::Close); }
                        *control_flow = ControlFlow::Exit;
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
        // 초기 워밍업
        let mut sys = System::new();
        sys.refresh_cpu();
        sys.refresh_memory();

        let mut networks = Networks::new_with_refreshed_list();
        networks.refresh();

        thread::sleep(Duration::from_millis(200));
        sys.refresh_cpu();

        let mut prev_net = snapshot_network(&networks);
        let mut prev_disk = snapshot_disk();
        let mut last = Instant::now();

        let mut max_disk_mbps = RollingMax::default();
        let mut max_net_kbps = RollingMax::default();

        loop {
            // 명령 처리(ResetDiffs 하나뿐)
            while cmd_rx.try_recv().is_ok() {
                networks.refresh();
                prev_net = snapshot_network(&networks);
                prev_disk = snapshot_disk();
                max_disk_mbps = RollingMax::default();
                max_net_kbps = RollingMax::default();
                last = Instant::now(); // 델타 기준도 리셋
            }

            // 주기 & 시스템 새로고침
            let now = Instant::now();
            let dt = (now - last).as_secs_f64();
            last = now;

            sys.refresh_cpu();
            sys.refresh_memory();
            networks.refresh();

            // 1) CPU / RAM: 항상 계산
            let cpu = sys.global_cpu_info().cpu_usage() as f32;
            let mem_pct = if sys.total_memory() > 0 {
                (sys.used_memory() as f32 / sys.total_memory() as f32) * 100.0
            } else { 0.0 };

            // 2) NET: 항상 계산
            let cur_net = snapshot_network(&networks);
            let (mut net_kbps, prev_net_out) = diff_network(prev_net, cur_net, dt.max(1e-3));
            prev_net = prev_net_out;

            // 3) DISK: 항상 계산
            let cur_disk = snapshot_disk();
            let (mut disk_mbps, prev_disk_out) = diff_disk(prev_disk, cur_disk, dt.max(1e-3));
            prev_disk = prev_disk_out;

            // 아주 작은 떨림 제거(노이즈 컷)
            if disk_mbps.is_finite() && disk_mbps < 0.001 { disk_mbps = 0.0; }
            if net_kbps.is_finite()  && net_kbps  < 0.1   { net_kbps  = 0.0; }

            // 4) 스케일 업데이트(막대 빈 현상 방지): 항상 계산
            let disk_floor = 0.25; // MB/s
            let net_floor  = 4.0;  // KB/s
            let disk_scale = max_disk_mbps.update(disk_mbps.max(disk_floor));
            let net_scale  = max_net_kbps.update(net_kbps .max(net_floor));
            let disk_pct = ((disk_mbps / disk_scale) * 100.0).clamp(0.0, 100.0);
            let net_pct  = ((net_kbps  / net_scale ) * 100.0).clamp(0.0, 100.0);

            // 5) 공유 메트릭: 매 틱 갱신 (오버레이는 모드 상관없이 항상 최신 4종 표시)
            {
                let mut sm = shared_metrics.lock().unwrap();
                sm.cpu_pct   = cpu;
                sm.ram_pct   = mem_pct;
                sm.disk_mbps = disk_mbps;
                sm.net_kbps  = net_kbps;
                sm.disk_pct  = disk_pct;
                sm.net_pct   = net_pct;
            }

            // 6) 트레이 아이콘/툴팁만 모드에 따라 표시
            let current_mode = *mode.lock().unwrap();
            match current_mode {
                Mode::CpuRam => {
                    let tooltip = format!("CPU: {:.1}% | RAM: {:.1}%", cpu, mem_pct);
                    let rgba = make_icon_bars(w, h, cpu as f64, mem_pct as f64, Palette::CpuRam);
                    let _ = proxy.send_event(AppEvent::Update { tooltip, rgba, w, h });
                }
                Mode::DiskNet => {
                    let tooltip = format!(
                        "Disk: {:.2} MB/s ({:.0}%) | Net: {:.0} KB/s ({:.0}%)",
                        disk_mbps, disk_pct, net_kbps, net_pct
                    );
                    let rgba = make_icon_bars(w, h, disk_pct, net_pct, Palette::DiskNet);
                    let _ = proxy.send_event(AppEvent::Update { tooltip, rgba, w, h });
                }
            }

            thread::sleep(Duration::from_millis(1000));
        }
    });
}

//
// ---------- Windows 전용: 오버레이 & 원시 API ----------
//
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
        // 1) 윈도우 클래스 등록
        let class_name: Vec<u16> = "CRMonitorOverlay\0".encode_utf16().collect();
        let h_instance = GetModuleHandleW(null_mut());

        extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            unsafe {
                match msg {
                    WM_NCHITTEST => {
                        // 마우스 좌표 (화면 기준)
                        let x = (l as u32 & 0xFFFF) as i16 as i32;
                        let y = (l as i32) >> 16;
                        let point = POINT { x, y };

                        let mut window_rect = zeroed();
                        GetWindowRect(hwnd, &mut window_rect);

                        // 핸들 영역: 창의 좌상단 16x16 영역 (화면 기준)
                        let handle_rect = RECT {
                            left: window_rect.left,
                            top: window_rect.top,
                            right: window_rect.left + 16,
                            bottom: window_rect.top + 16,
                        };

                        // 마우스 포인터가 핸들 영역 안에 있으면 HTCAPTION을 반환하여 창을 드래그할 수 있게 함
                        if PtInRect(&handle_rect, point) != 0 {
                            return HTCAPTION as isize;
                        }
                        // 그렇지 않으면 HTTRANSPARENT를 반환하여 클릭 이벤트를 통과시킴
                        return HTTRANSPARENT as isize;
                    }
                    WM_PAINT => {
                        let mut ps: PAINTSTRUCT = zeroed();
                        let hdc = BeginPaint(hwnd, &mut ps);

                        // 배경(컬러키 색으로 지우기 → 완전 투명 처리됨)
                        let rect = RECT { left: 0, top: 0, right: 240, bottom: 120 };
                        let brush = CreateSolidBrush(COLKEY);
                        FillRect(hdc, &rect, brush);
                        DeleteObject(brush as _);

                        // 핸들 그리기 (좌상단 회색 점)
                        let handle_rect = RECT { left: 0, top: 0, right: 16, bottom: 16 };
                        let handle_brush = CreateSolidBrush(rgb(100, 100, 100));
                        FillRect(hdc, &handle_rect, handle_brush);
                        DeleteObject(handle_brush as _);

                        SetBkMode(hdc, TRANSPARENT as i32);
                        SetTextColor(hdc, rgb(200, 200, 200));

                        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Metrics;
                        if !ptr.is_null() {
                            let m = *ptr;
                            let mut y = 6;
                            draw_text_shadowed(hdc, 8, y, "--System Usage--");
                            y += 20;
                            draw_text_shadowed(hdc, 8, y, &format!("CPU: {:.1} %", m.cpu_pct));
                            y += 18;
                            draw_text_shadowed(hdc, 8, y, &format!("RAM: {:.1} %", m.ram_pct));
                            y += 18;
                            draw_text_shadowed(
                                hdc,
                                8,
                                y,
                                &format!("DISK: {:.2} MB/s ({:.0} %)", m.disk_mbps, m.disk_pct),
                            );
                            y += 18;
                            draw_text_shadowed(
                                hdc,
                                8,
                                y,
                                &format!("NET: {:.0} KB/s ({:.0} %)", m.net_kbps, m.net_pct),
                            );
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

        #[cfg(windows)]
        #[cfg(windows)]
        unsafe fn draw_text_shadowed(
            hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            x: i32,
            y: i32,
            s: &str,
        ) {
            use windows_sys::Win32::Graphics::Gdi::{SetTextColor, TextOutW};

            let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();

            let prev = SetTextColor(hdc, rgb(255, 255, 255)); // 회색 본문
            TextOutW(hdc, x, y, w.as_ptr(), (w.len() - 1) as i32);
            SetTextColor(hdc, prev);
        }


        let mut wc: WNDCLASSW = zeroed();
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = h_instance;
        wc.lpszClassName = class_name.as_ptr();
        wc.hCursor = LoadCursorW(0, IDC_ARROW);

        if RegisterClassW(&wc) == 0 {
            // NOTE: This can fail if the class is already registered, which is fine.
            // A real app might check GetLastError for ERROR_CLASS_ALREADY_EXISTS.
        }

        // 2) 창 생성 (저장된 위치 또는 기본 위치)
        let initial_pos = *position.lock().unwrap();
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED, // WS_EX_TOPMOST 제거하여 다른 창에 가려질 수 있도록 함
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_POPUP,
            initial_pos.x,
            initial_pos.y,
            240,
            120,
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

        // Metrics 포인터 저장
        let mut boxed_metrics = Box::new(*shared.lock().unwrap());
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

            // 최신 메트릭 반영 + 다시 그리기
            *boxed_metrics = *shared.lock().unwrap();
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

//
// ---------- Windows 전용: 정확한 NET/DISK 스냅샷 ----------
//
#[cfg(windows)]
fn snapshot_network_win() -> NetBytes {
    use std::ptr::null_mut;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2, IF_TYPE_SOFTWARE_LOOPBACK,
    };
    const NET_IF_OPER_STATUS_UP: u32 = 1;

    unsafe {
        let mut table_ptr: *mut MIB_IF_TABLE2 = null_mut();
        if GetIfTable2(&mut table_ptr) != 0 || table_ptr.is_null() {
            return NetBytes { rx: 0, tx: 0 };
        }
        let table = &*table_ptr;
        let first: *const MIB_IF_ROW2 = table.Table.as_ptr();

        let mut rx = 0u64;
        let mut tx = 0u64;
        for i in 0..(table.NumEntries as usize) {
            let row = &*first.add(i);
            if (row.OperStatus as u32) == NET_IF_OPER_STATUS_UP && row.Type != IF_TYPE_SOFTWARE_LOOPBACK {
                rx = rx.saturating_add(row.InOctets);
                tx = tx.saturating_add(row.OutOctets);
            }
        }
        FreeMibTable(table_ptr as *mut _);
        NetBytes { rx, tx }
    }
}


fn snapshot_network(_networks: &Networks) -> NetBytes {
    #[cfg(windows)]
    { return snapshot_network_win(); }

}


#[cfg(windows)]
fn snapshot_disk_win() -> DiskBytes {
    use std::ffi::OsString;
    use std::mem::{size_of, zeroed};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{*};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_READONLY, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    // Ntdddisk.h 의 IOCTL_DISK_PERFORMANCE (직접 정의)
    const IOCTL_DISK_PERFORMANCE: u32 = 0x70020;

    #[repr(C)]
    #[allow(non_camel_case_types)]
    struct DISK_PERFORMANCE {
        bytes_read: i64,
        bytes_written: i64,
        _rest: [u8; 200], // 나머지 필드는 사용하지 않음
    }

    // 표준 OpenOptions로 \\.\PhysicalDriveN 열기
    fn open_physical(n: u32) -> Option<std::fs::File> {
        let path = OsString::from(format!(r"\\.\\PhysicalDrive{}", n));
        let mut opts = std::fs::OpenOptions::new();
        let file = opts
            .read(true)
            // Windows 전용 확장: 공유/접근/속성 지정
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .access_mode(FILE_GENERIC_READ)
            .attributes(FILE_ATTRIBUTE_READONLY)
            .open(PathBuf::from(path))
            .ok()?;
        Some(file)
    }

    let mut total_r: u64 = 0;
    let mut total_w: u64 = 0;

    for n in 0..32 {
        let file = match open_physical(n) {
            Some(f) => f,
            None => continue,
        };

        // std 핸들을 WinAPI HANDLE로 변환
        let h: HANDLE = file.as_raw_handle() as HANDLE;

        unsafe {
            let mut perf: DISK_PERFORMANCE = zeroed();
            let mut ret = 0u32;
            let ok = DeviceIoControl(
                h,
                IOCTL_DISK_PERFORMANCE,
                std::ptr::null_mut(),
                0,
                &mut perf as *mut _ as *mut _,
                size_of::<DISK_PERFORMANCE>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            );
            // file은 여기서 drop; CloseHandle 불필요(표준 파일이 닫으면서 정리)

            if ok != 0 {
                if perf.bytes_read > 0 {
                    total_r = total_r.saturating_add(perf.bytes_read as u64);
                }
                if perf.bytes_written > 0 {
                    total_w = total_w.saturating_add(perf.bytes_written as u64);
                }
            }
        }
    }

    DiskBytes { read: total_r, write: total_w }
}



fn snapshot_disk() -> DiskBytes {
    cfg_if::cfg_if! {
        if #[cfg(all(target_os = "linux", feature = "linux-disk"))] {
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
        } else if #[cfg(windows)] {
            return snapshot_disk_win();
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

fn diff_disk(prev: DiskBytes, cur: DiskBytes, dt: f64) -> (f64, DiskBytes) {
    let dread  = cur.read .saturating_sub(prev.read);
    let dwrite = cur.write.saturating_sub(prev.write);
    let mbps = if dt > 0.0 { (dread as f64 + dwrite as f64) / dt / (1024.0 * 1024.0) } else { 0.0 };
    (mbps, cur)
}


//
// ---------- 아이콘 그리기 ----------
//
#[derive(Clone, Copy)]
enum Palette { CpuRam, DiskNet }

fn make_icon_bars(w: u32, h: u32, v1_pct: f64, v2_pct: f64, palette: Palette) -> Vec<u8> {
    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, Rgba([0, 0, 0, 0]));

    // 안전 마진과 최소 폭/높이 보정
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
