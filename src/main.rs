mod app;
mod decode;
mod mcp;
mod protocol;
mod session;
mod vnc;

use eframe::egui;

use crate::app::DeviceHubApp;
use crate::protocol::{
    ActiveSlot, ClipboardSlot, ControlCmd, DeviceListSlot, ErrorSlot, FrameSlot, InputSink,
    OrientationSlot, StatusSlot, VncControl,
};

/// Parsed command line: which device, and whether to run with the GUI or as a
/// headless server (for running on a box with no display).
struct Cli {
    udid: Option<String>,
    headless: bool,
    /// Desired VNC server state. In GUI mode this seeds the toggle (and is
    /// otherwise driven by persisted settings); in headless mode it's the final
    /// word, since there's no UI to flip it.
    vnc_enabled: bool,
    vnc_addr: String,
    vnc_password: String,
    /// Whether to run the MCP server, and where to bind it.
    mcp_enabled: bool,
    mcp_addr: Option<String>,
}

const HELP: &str = "\
DeviceHub - live iOS screen mirror + remote control over CoreDevice

USAGE:
    devicehub_rs [OPTIONS] [UDID]

ARGS:
    <UDID>    Device to connect to. Defaults to the first attached device.

OPTIONS:
    --headless            Run without the GUI (for servers / SSH). Enables the
                          VNC and MCP servers by default so the device is
                          reachable; toggle them with the flags below.
    --vnc [ADDR]          Enable the VNC server (default bind 127.0.0.1:5900).
    --no-vnc              Disable the VNC server (headless only).
    --vnc-password <PW>   VNC password. Empty means no auth (trusted nets only).
    --mcp [ADDR]          Enable the MCP server (default bind 127.0.0.1:8009).
    --no-mcp              Disable the MCP server.
    -h, --help            Print this help.

Binding to 0.0.0.0 exposes the device beyond loopback. The VNC password is the
only auth; the MCP server has none, so keep it on loopback or a trusted network.
";

const DEFAULT_VNC_ADDR: &str = "127.0.0.1:5900";

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,devicehub_rs=info".into()),
        )
        .init();

    let cli = match parse_args() {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("{msg}\n\n{HELP}");
            std::process::exit(2);
        }
    };

    if cli.headless {
        run_headless(cli);
        Ok(())
    } else {
        run_gui(cli)
    }
}

/// Parse `std::env::args`. Returns `Err` with a message on a bad flag; the
/// `--help` request is handled by printing and exiting directly.
fn parse_args() -> Result<Cli, String> {
    let mut udid = None;
    let mut headless = false;
    let mut vnc_flag: Option<bool> = None;
    let mut vnc_addr = DEFAULT_VNC_ADDR.to_string();
    let mut vnc_password = String::new();
    let mut mcp_flag: Option<bool> = None;
    let mut mcp_addr = None;

    // A value follows the flag only when the next arg isn't itself a flag.
    fn take_value(args: &mut std::iter::Peekable<impl Iterator<Item = String>>) -> Option<String> {
        match args.peek() {
            Some(next) if !next.starts_with("--") => args.next(),
            _ => None,
        }
    }

    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--headless" => headless = true,
            "--vnc" => {
                vnc_flag = Some(true);
                if let Some(addr) = take_value(&mut args) {
                    vnc_addr = addr;
                }
            }
            "--no-vnc" => vnc_flag = Some(false),
            "--vnc-password" => {
                vnc_password = args
                    .next()
                    .ok_or_else(|| "--vnc-password needs a value".to_string())?;
            }
            "--mcp" => {
                mcp_flag = Some(true);
                if let Some(addr) = take_value(&mut args) {
                    mcp_addr = Some(addr);
                }
            }
            "--no-mcp" => mcp_flag = Some(false),
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            _ => {
                if udid.replace(arg).is_some() {
                    return Err("more than one UDID given".to_string());
                }
            }
        }
    }

    // In headless mode VNC/MCP default on (there's no UI to enable them); in GUI
    // mode VNC defaults off (persisted settings drive it) and MCP stays on as
    // before. An explicit flag always wins.
    let vnc_enabled = vnc_flag.unwrap_or(headless);
    let mcp_enabled = mcp_flag.unwrap_or(true);

    Ok(Cli {
        udid,
        headless,
        vnc_enabled,
        vnc_addr,
        vnc_password,
        mcp_enabled,
        mcp_addr,
    })
}

/// All the shared state the device session, VNC server and MCP server read and
/// write. Created once and handed to both the workers and (in GUI mode) the UI.
#[derive(Clone, Default)]
struct Slots {
    frames: FrameSlot,
    status: StatusSlot,
    clipboard: ClipboardSlot,
    orientation: OrientationSlot,
    device_list: DeviceListSlot,
    active: ActiveSlot,
    error: ErrorSlot,
    input_sink: InputSink,
}

/// Spawn the device-session thread: it owns a tokio runtime running the VNC
/// supervisor, the MCP server and the session manager, all wired to `slots`.
/// `repaint` is pulsed when a new frame lands (a no-op in headless mode).
fn spawn_workers(
    udid: Option<String>,
    repaint: impl Fn() + Send + Clone + 'static,
    slots: Slots,
    vnc: VncControl,
    mcp_enabled: bool,
    control_tx: tokio::sync::mpsc::UnboundedSender<ControlCmd>,
    control_rx: tokio::sync::mpsc::UnboundedReceiver<ControlCmd>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("device-session".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime");
            rt.block_on(async move {
                tokio::spawn(vnc::supervise(
                    vnc,
                    slots.frames.clone(),
                    slots.input_sink.clone(),
                    slots.orientation.clone(),
                ));

                if mcp_enabled {
                    tokio::spawn(mcp::serve(
                        slots.frames.clone(),
                        slots.input_sink.clone(),
                        slots.orientation.clone(),
                        slots.device_list.clone(),
                        slots.active.clone(),
                        slots.error.clone(),
                        slots.status.clone(),
                        control_tx,
                    ));
                }

                session::manage(
                    udid,
                    repaint,
                    slots.frames,
                    slots.status,
                    slots.clipboard,
                    slots.orientation,
                    slots.device_list,
                    slots.active,
                    slots.error,
                    slots.input_sink,
                    control_rx,
                )
                .await;
            });
        })
        .expect("spawn session thread")
}

/// Headless entry point: no eframe, no window. Bring up the workers with the
/// CLI-configured VNC/MCP servers and block until the session thread exits.
fn run_headless(cli: Cli) {
    if cli.mcp_enabled && let Some(addr) = &cli.mcp_addr {
        // `mcp::serve` reads its bind address from this env var.
        unsafe { std::env::set_var("DEVICEHUB_MCP_ADDR", addr) };
    }

    let slots = Slots::default();
    let vnc = VncControl::seeded(cli.vnc_enabled, cli.vnc_addr, cli.vnc_password);
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel::<ControlCmd>();

    tracing::info!(
        "starting headless (vnc: {}, mcp: {})",
        if cli.vnc_enabled { "on" } else { "off" },
        if cli.mcp_enabled { "on" } else { "off" },
    );

    let handle = spawn_workers(
        cli.udid,
        || {},
        slots,
        vnc,
        cli.mcp_enabled,
        control_tx,
        control_rx,
    );
    let _ = handle.join();
}

/// GUI entry point: run eframe, spawning the workers once its context exists.
fn run_gui(cli: Cli) -> eframe::Result<()> {
    let slots = Slots::default();
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel::<ControlCmd>();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DeviceHub")
            .with_inner_size([520.0, 920.0]),
        ..Default::default()
    };

    eframe::run_native(
        "DeviceHub",
        native_options,
        Box::new(move |cc| {
            // The egui Context only exists once eframe is up; spawn the session here.
            let repaint_ctx = cc.egui_ctx.clone();
            // Seeded from persisted settings, unless a CLI flag overrode them.
            let vnc = vnc_settings(cc.storage, &cli);

            let session_thread = spawn_workers(
                cli.udid,
                move || repaint_ctx.request_repaint(),
                slots.clone(),
                vnc.clone(),
                cli.mcp_enabled,
                control_tx.clone(),
                control_rx,
            );

            Ok(Box::new(DeviceHubApp::new(
                slots.frames,
                slots.status,
                slots.clipboard,
                slots.orientation,
                slots.device_list,
                slots.active,
                slots.error,
                slots.input_sink,
                vnc,
                control_tx,
                session_thread,
            )) as Box<dyn eframe::App>)
        }),
    )
}

/// Build the initial [`VncControl`] from persisted settings, defaulting to loopback
/// with no auth. CLI flags (`--vnc`, `--vnc-password`) override the stored values.
fn vnc_settings(storage: Option<&dyn eframe::Storage>, cli: &Cli) -> VncControl {
    let get = |key: &str| storage.and_then(|s| eframe::get_value::<String>(s, key));
    let host = get("vnc_host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = get("vnc_port").unwrap_or_else(|| "5900".to_string());
    let stored_addr = format!("{host}:{port}");
    let stored_password = get("vnc_password").unwrap_or_default();
    let stored_enabled = storage
        .and_then(|s| eframe::get_value::<bool>(s, "vnc_enabled"))
        .unwrap_or(false);

    // A `--vnc ADDR` / `--vnc-password` on the command line wins over storage.
    let addr = if cli.vnc_addr == DEFAULT_VNC_ADDR {
        stored_addr
    } else {
        cli.vnc_addr.clone()
    };
    let password = if cli.vnc_password.is_empty() {
        stored_password
    } else {
        cli.vnc_password.clone()
    };
    let enabled = cli.vnc_enabled || stored_enabled;

    VncControl::seeded(enabled, addr, password)
}
