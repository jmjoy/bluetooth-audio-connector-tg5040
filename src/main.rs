use anyhow::anyhow;
use arc_swap::ArcSwap;
use bluer::{Adapter, AdapterEvent, Address, DeviceProperty};
use crossbeam::atomic::AtomicCell;
use sdl2::{
    controller::Button,
    event::Event,
    keyboard::Keycode,
    pixels::Color,
    rect::Rect,
    render::{TextureCreator, TextureQuery, WindowCanvas},
    ttf::Font,
    video::WindowContext,
};
use std::{env, ops::Deref, pin::pin, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, Mutex},
    time::{sleep, timeout},
};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

const SCREEN_WIDTH: u32 = 1280;
const SCREEN_HEIGHT: u32 = 720;

const PADDING: u32 = 32;

// handle the annoying Rect i32
macro_rules! rect(
    ($x:expr, $y:expr, $w:expr, $h:expr) => (
        Rect::new($x as i32, $y as i32, $w as u32, $h as u32)
    )
);

#[derive(PartialEq, Clone, Copy)]
enum BluetoothScanStatus {
    Disable,
    Scanning,
    Finished,
    Failed,
}

#[derive(PartialEq, Clone)]
enum BluetoothConnectStatus {
    Disable,
    Connecting,
    Finished,
    Failed { reason: String },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // if env::var("RUST_BACKTRACE").is_err() {
    //     env::set_var("RUST_BACKTRACE", "1");
    // }

    // a builder for `FmtSubscriber`.
    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(Level::DEBUG)
        // completes the builder.
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    let sdl_context = sdl2::init().map_err(anyhow::Error::msg)?;

    let video_subsystem = sdl_context.video().map_err(anyhow::Error::msg)?;

    let window = video_subsystem
        .window(env!("CARGO_CRATE_NAME"), SCREEN_WIDTH, SCREEN_HEIGHT)
        .position_centered()
        .build()?;

    let game_controller_subsystem = sdl_context.game_controller().map_err(anyhow::Error::msg)?;
    let game_controller = if game_controller_subsystem
        .num_joysticks()
        .map_err(anyhow::Error::msg)?
        > 0
    {
        Some(game_controller_subsystem.open(0)?)
    } else {
        warn!("no game controller found");
        None
    };

    if let Some(game_controller) = &game_controller {
        debug!(mapping = game_controller.mapping(), "controller mapping");
    }

    let ttf_context = sdl2::ttf::init()?;
    let font = ttf_context
        .load_font("wqy-microhei.ttc", 30)
        .map_err(anyhow::Error::msg)?;

    let mut canvas = window.into_canvas().build()?;
    let texture_creator = canvas.texture_creator();

    canvas.set_draw_color(Color::RGB(255, 255, 255));
    canvas.clear();
    canvas.present();

    let mut event_pump = sdl_context.event_pump().map_err(anyhow::Error::msg)?;

    let session = bluer::Session::new().await?;
    let adapter = Arc::new(session.default_adapter().await?);

    let mut is_running = true;
    let mut quit_count = 0;
    let mut is_bluetooth_powered = adapter.is_powered().await?;

    let bluetooth_scan_status = Arc::new(AtomicCell::new(BluetoothScanStatus::Disable));
    let bluetooth_devices = Arc::new(ArcSwap::new(Arc::new(Vec::new())));
    let mut selected_bluetooth_device_index = 0;
    let bluetooth_connect_status = Arc::new(Mutex::new(BluetoothConnectStatus::Disable));

    let (bluetooth_discover_devices_tx, bluetooth_discover_devices_rx) = mpsc::channel(1);

    background_discover_devices(
        adapter.clone(),
        bluetooth_scan_status.clone(),
        bluetooth_devices.clone(),
        bluetooth_discover_devices_rx,
        bluetooth_connect_status.clone(),
    );

    if is_bluetooth_powered {
        let _ = bluetooth_discover_devices_tx.try_send(());
    }

    let (bluetooth_connect_device_tx, bluetooth_connect_device_rx) = mpsc::channel(1);

    background_connect_device(
        adapter.clone(),
        bluetooth_connect_device_rx,
        bluetooth_devices.clone(),
        bluetooth_connect_status.clone(),
    );

    let mut text_drawer = TextDrawer {
        canvas,
        texture_creator,
        font,
    };

    'main_loop: loop {
        text_drawer.clear();

        let current_bluetooth_scan_status = bluetooth_scan_status.load();

        if is_running {
            for event in event_pump.poll_iter() {
                match event {
                    // 退出程序
                    Event::Quit { .. } |
                    Event::KeyUp { keycode: Some(Keycode::Escape), ..  } |
                    Event::KeyUp { keycode: Some(Keycode::B), ..  } |
                    Event::ControllerButtonUp { button: Button::A, .. } /* B of tg5040 */ => {
                        is_running = false;
                    }

                    // 打开蓝牙
                    Event::KeyUp {keycode: Some(Keycode::Y), .. } |
                    Event::ControllerButtonUp { button: Button::X, .. } /* Y of tg5040 */ => {
                        if is_bluetooth_powered {
                            continue;
                        }
                        info!("open bluetooth");
                        adapter.set_powered(true).await?;
                        is_bluetooth_powered = true;
                        bluetooth_scan_status.store(BluetoothScanStatus::Disable);
                        selected_bluetooth_device_index = 0;

                        let _ = bluetooth_discover_devices_tx.try_send(());
                    }

                    // 关闭蓝牙
                    Event::KeyUp {keycode: Some(Keycode::X), .. } |
                    Event::ControllerButtonUp { button: Button::Y, .. } /* X of tg5040 */ => {
                        if !is_bluetooth_powered {
                            continue;
                        }
                        info!("close bluetooth");
                        adapter.set_powered(false).await?;
                        is_bluetooth_powered = false;
                        bluetooth_scan_status.store(BluetoothScanStatus::Disable);
                        selected_bluetooth_device_index = 0;
                    }

                    // 选择蓝牙
                    Event::KeyUp {keycode: Some(Keycode::Up), .. } |
                    Event::ControllerButtonUp { button: Button::DPadUp, .. } => {
                        if current_bluetooth_scan_status != BluetoothScanStatus::Finished {
                            continue;
                        }
                        if selected_bluetooth_device_index == 0 {
                            selected_bluetooth_device_index = (&*bluetooth_devices).load().len() - 1;
                        } else {
                            selected_bluetooth_device_index -= 1;
                        }
                    }

                    // 选择蓝牙
                    Event::KeyUp {keycode: Some(Keycode::Down), .. } |
                    Event::ControllerButtonUp { button: Button::DPadDown, .. } => {
                        if current_bluetooth_scan_status != BluetoothScanStatus::Finished {
                            continue;
                        }
                        if selected_bluetooth_device_index == (&*bluetooth_devices).load().len() - 1 {
                            selected_bluetooth_device_index = 0;
                        } else {
                            selected_bluetooth_device_index += 1;
                        }
                    }

                    Event::KeyUp {keycode: Some(Keycode::A), .. } |
                    Event::ControllerButtonUp { button: Button::B, .. } => /* A of tg5040 */{
                        if current_bluetooth_scan_status != BluetoothScanStatus::Finished {
                            continue;
                        }
                        if *bluetooth_connect_status.lock().await == BluetoothConnectStatus::Connecting {
                            continue;
                        }

                        let _ = bluetooth_connect_device_tx.try_send(selected_bluetooth_device_index);
                    }

                    _ => {}
                }
            }
        }

        if !is_running {
            text_drawer.draw("退出中……", Color::RGB(255, 0, 0), PADDING, PADDING)?;
        } else {
            let (_, b_height) = text_drawer.draw("按B退出程序。", Color::RGB(0, 0, 0), 0, 0)?;

            let (last_width, last_height) = text_drawer.draw(
                "按Y打开蓝牙，按X关闭蓝牙。当前蓝牙状态：",
                Color::RGB(0, 0, 0),
                0,
                b_height,
            )?;

            if is_bluetooth_powered {
                text_drawer.draw("开", Color::RGB(0, 255, 0), last_width, b_height)?;
            } else {
                text_drawer.draw("关", Color::RGB(255, 0, 0), last_width, b_height)?;
            }

            let (_, last_height) = match current_bluetooth_scan_status {
                BluetoothScanStatus::Disable => {
                    text_drawer.draw(" ", Color::RGB(0, 0, 0), 0, last_height)?
                }
                BluetoothScanStatus::Scanning => {
                    text_drawer.draw("扫描中……", Color::RGB(0, 0, 255), 0, last_height)?
                }
                BluetoothScanStatus::Finished => {
                    let (success_width, success_height) =
                        text_drawer.draw("扫描成功", Color::RGB(0, 255, 0), 0, last_height)?;

                    if let Some(info) = (&*bluetooth_devices)
                        .load()
                        .iter()
                        .find(|info| info.connected)
                    {
                        text_drawer.draw(
                            &format!("已连接：{}", &info.name),
                            Color::RGB(100, 100, 100),
                            success_width,
                            last_height,
                        )?;
                    } else {
                        text_drawer.draw(
                            "未连接蓝牙",
                            Color::RGB(100, 100, 100),
                            success_width,
                            last_height,
                        )?;
                    }

                    (success_width, success_height)
                }
                BluetoothScanStatus::Failed => {
                    text_drawer.draw("扫描失败", Color::RGB(255, 0, 0), 0, last_height)?
                }
            };

            if current_bluetooth_scan_status == BluetoothScanStatus::Finished {
                let (_, last_height) = text_drawer.draw(
                    &format!("使用 ↑↓ 选择蓝牙设备，按A连接。当前设备："),
                    Color::RGB(0, 0, 0),
                    0,
                    last_height,
                )?;

                let devices = (&*bluetooth_devices).load();
                let device = &devices[selected_bluetooth_device_index];
                let show_name = if device.name == "" {
                    device.addr.to_string()
                } else {
                    device.name.to_string()
                };

                let (_, last_height) = text_drawer.draw(
                    &format!(
                        "（{}/{}） {}",
                        selected_bluetooth_device_index + 1,
                        devices.len(),
                        show_name
                    ),
                    Color::RGB(100, 100, 100),
                    0,
                    last_height,
                )?;

                match &*bluetooth_connect_status.lock().await {
                    BluetoothConnectStatus::Disable => {
                        text_drawer.draw(" ", Color::RGB(0, 0, 0), 0, last_height)?;
                    }
                    BluetoothConnectStatus::Connecting => {
                        text_drawer.draw("连接中……", Color::RGB(0, 0, 255), 0, last_height)?;
                    }
                    BluetoothConnectStatus::Finished => {
                        text_drawer.draw("连接成功", Color::RGB(0, 255, 0), 0, last_height)?;
                    }
                    BluetoothConnectStatus::Failed { reason } => {
                        text_drawer.draw(
                            &format!("连接失败：{}", reason),
                            Color::RGB(255, 0, 0),
                            0,
                            last_height,
                        )?;
                    }
                }
            }
        }

        text_drawer.present();

        sleep(Duration::new(0, 1_000_000_000u32 / 60)).await;

        if !is_running {
            quit_count += 1;
            if quit_count > 3 {
                break 'main_loop;
            }
        }
    }

    Ok(())
}

fn background_discover_devices(
    adapter: Arc<Adapter>, bluetooth_scan_status: Arc<AtomicCell<BluetoothScanStatus>>,
    bluetooth_devices: Arc<ArcSwap<Vec<BluetoothDeviceInfo>>>,
    mut bluetooth_discover_devices_rx: mpsc::Receiver<()>,
    bluetooth_connect_status: Arc<Mutex<BluetoothConnectStatus>>,
) {
    tokio::spawn(async move {
        loop {
            if bluetooth_discover_devices_rx.recv().await.is_none() {
                break;
            }

            if let Err(err) = async {
                bluetooth_scan_status.store(BluetoothScanStatus::Scanning);

                let device_events = adapter.discover_devices().await?;
                let mut device_events = pin!(device_events);

                let mut devices = Vec::new();

                let _ = timeout(Duration::from_secs(6), async {
                    while let Some(device_event) = device_events.next().await {
                        match device_event {
                            AdapterEvent::DeviceAdded(addr) => {
                                let device = match adapter.device(addr) {
                                    Ok(device) => device,
                                    Err(err) => {
                                        error!(?err, "get device failed");
                                        continue;
                                    }
                                };
                                let properties = match device.all_properties().await {
                                    Ok(properties) => properties,
                                    Err(err) => {
                                        error!(?err, "get device properties failed");
                                        continue;
                                    }
                                };

                                let mut info = BluetoothDeviceInfo::default();
                                info.addr = addr;

                                for prop in properties {
                                    match prop {
                                        DeviceProperty::Name(name) => {
                                            info.name = name;
                                        }
                                        DeviceProperty::Paired(paired) => {
                                            info.paired = paired;
                                        }
                                        DeviceProperty::Connected(connected) => {
                                            info.connected = connected;
                                        }
                                        _ => {}
                                    }
                                }

                                devices.push(info);
                            }
                            AdapterEvent::DeviceRemoved(addr) => {
                                for (index, device) in devices.iter().enumerate() {
                                    if &device.addr == &addr {
                                        devices.remove(index);
                                        break;
                                    }
                                }
                            }
                            _ => (),
                        }
                    }
                })
                .await;

                if devices.iter().find(|info| info.connected).is_some() {
                    *bluetooth_connect_status.lock().await = BluetoothConnectStatus::Finished;
                }

                bluetooth_devices.store(Arc::new(devices));

                bluetooth_scan_status.store(BluetoothScanStatus::Finished);
                anyhow::Ok(())
            }
            .await
            {
                error!(?err, "discover devices failed");
                bluetooth_scan_status.store(BluetoothScanStatus::Failed);
            }
        }
    });
}

struct TextDrawer<'ttf_module, 'rwops> {
    canvas: WindowCanvas,
    texture_creator: TextureCreator<WindowContext>,
    font: Font<'ttf_module, 'rwops>,
}

impl<'ttf_module, 'rwops> TextDrawer<'ttf_module, 'rwops> {
    fn draw(&mut self, text: &str, color: Color, x: u32, y: u32) -> anyhow::Result<(u32, u32)> {
        let surface = self.font.render(text).blended(color)?;
        let texture = self.texture_creator.create_texture_from_surface(&surface)?;
        let TextureQuery { width, height, .. } = texture.query();
        let target = rect!(PADDING + x, PADDING + y, width, height);
        if let Err(err) = self.canvas.copy(&texture, None, Some(target)) {
            return Err(anyhow!("{}", err));
        }
        Ok((PADDING + x + width, PADDING + y + height))
    }

    fn clear(&mut self) {
        self.canvas.clear();
    }

    fn present(&mut self) {
        self.canvas.present();
    }
}

#[derive(Default, Clone)]
struct BluetoothDeviceInfo {
    addr: Address,
    name: String,
    paired: bool,
    connected: bool,
}

fn background_connect_device(
    adapter: Arc<Adapter>, mut rx: mpsc::Receiver<usize>,
    bluetooth_devices: Arc<ArcSwap<Vec<BluetoothDeviceInfo>>>,
    bluetooth_connect_status: Arc<Mutex<BluetoothConnectStatus>>,
) {
    tokio::spawn(async move {
        loop {
            let Some(selected_bluetooth_device_index) = rx.recv().await else {
                break;
            };

            if let Err(err) = async {
                *bluetooth_connect_status.lock().await = BluetoothConnectStatus::Connecting;

                let mut device_infos = bluetooth_devices.deref().load().deref().deref().clone();

                // 先断开之前的连接
                for device_info in &mut device_infos {
                    if !device_info.connected {
                        continue;
                    }
                    let device = adapter.device(device_info.addr.clone())?;
                    device.disconnect().await?;
                    device_info.connected = false;
                }

                // 再重新连接
                let device = adapter.device(device_infos[selected_bluetooth_device_index].addr)?;

                if !device.is_paired().await? {
                    device.pair().await?;
                }

                if !device.is_connected().await? {
                    device.connect().await?;
                }

                device_infos[selected_bluetooth_device_index].connected = true;

                bluetooth_devices.store(Arc::new(device_infos));

                *bluetooth_connect_status.lock().await = BluetoothConnectStatus::Finished;

                anyhow::Ok(())
            }
            .await
            {
                error!(?err, "connect device failed");
                *bluetooth_connect_status.lock().await = BluetoothConnectStatus::Failed {
                    reason: err.to_string(),
                };
            }
        }
    });
}
