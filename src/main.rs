use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use idevice::{
    Idevice, IdeviceService,
    core_device_proxy::CoreDeviceProxy,
    house_arrest::HouseArrestClient,
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    pairing_file::PairingFile,
    provider::IdeviceProvider,
    remote_pairing::{RemotePairingClient, RpPairingFile},
    rsd::RsdHandshake,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
    RemoteXpcClient,
};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

mod discover;
mod mount;

const RP_PAIRING_FILE_NAME: &str = "rp_pairing_file.plist";

#[derive(Parser)]
#[command(name = "idevice_pair")]
#[command(about = "CLI tool to pair iOS devices and install pairing files", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List connected iOS devices
    List,
    /// Pair with a device (Lockdown or Remote)
    Pair {
        /// UDID of the device (if not provided, uses the first one found)
        #[arg(short, long)]
        udid: Option<String>,
        /// Use Remote Pairing instead of Lockdown
        #[arg(short, long)]
        remote: bool,
        /// Save the pairing file to this path
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Validate an existing pairing record
    Validate {
        /// Path to the pairing file (.plist)
        #[arg(short, long)]
        file: PathBuf,
        /// IP address of the device (for network validation)
        #[arg(short, long)]
        ip: Option<IpAddr>,
        /// Use Remote Pairing validation
        #[arg(short, long)]
        remote: bool,
        /// UDID of the device (required for remote validation)
        #[arg(short, long)]
        udid: Option<String>,
    },
    /// List installed apps on the device
    ListApps {
        #[arg(short, long)]
        udid: Option<String>,
    },
    /// Install a pairing file to a specific app's Documents folder
    Install {
        #[arg(short, long)]
        udid: Option<String>,
        /// Path to the pairing file to install
        #[arg(short, long)]
        file: PathBuf,
        /// Bundle ID of the target app
        #[arg(short, long)]
        bundle_id: String,
        /// Filename in the app's Documents folder
        #[arg(short, long)]
        name: String,
    },
    /// Mount Developer Disk Image (DDI)
    Mount {
        #[arg(short, long)]
        udid: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::List => {
            let devices = get_all_devices().await?;
            if devices.is_empty() {
                println!("No devices found.");
            } else {
                println!("{:<40} {:<15} {:<10}", "UDID", "Type", "ID");
                for dev in devices {
                    println!("{:<40} {:<15} {:<10}", dev.udid, format!("{:?}", dev.connection_type), dev.device_id);
                }
            }
        }
        Commands::Pair { udid, remote, output } => {
            let dev = get_device(udid).await?;
            let provider = dev.to_provider(UsbmuxdAddr::default(), "idevice_pair");
            
            if remote {
                println!("Starting Remote Pairing... Please trust the device if prompted.");
                let hostname = pairing_hostname();
                let pairing_file = generate_remote_pairing_file(&provider, &hostname).await?;
                let bytes = pairing_file.to_bytes();
                save_pairing_data(bytes, output, &format!("{}_remote.plist", dev.udid))?;
            } else {
                println!("Starting Lockdown Pairing...");
                let mut uc = connect_to_usbmuxd().await?;
                let buid = uc.get_buid().await.context("Failed to get BUID")?;
                
                let mut buid_chars: Vec<char> = buid.chars().collect();
                if !buid_chars.is_empty() {
                    buid_chars[0] = if buid_chars[0] == 'F' { 'A' } else { 'F' };
                }
                let buid: String = buid_chars.into_iter().collect();
                
                let mut lc = LockdownClient::connect(&provider).await.context("Failed to connect to Lockdown")?;
                let id = uuid::Uuid::new_v4().to_string().to_uppercase();
                let pairing_file = lc.pair(id, buid, None).await.context("Pairing failed")?;
                let bytes = pairing_file.serialize().context("Failed to serialize pairing file")?;
                save_pairing_data(bytes, output, &format!("{}.plist", dev.udid))?;
            }
        }
        Commands::Validate { file, ip, remote, udid } => {
            let data = std::fs::read(&file).context("Failed to read pairing file")?;
            if remote {
                let dev = get_device(udid).await?;
                let provider = dev.to_provider(UsbmuxdAddr::default(), "idevice_pair");
                let mut pairing_file = RpPairingFile::from_bytes(&data).context("Invalid remote pairing file")?;
                
                println!("Validating Remote Pairing...");
                validate_remote(&provider, &mut pairing_file).await?;
                println!("Remote Pairing is VALID.");
            } else {
                let pairing_file = PairingFile::from_bytes(&data).context("Invalid lockdown pairing file")?;
                let target_ip = match ip {
                    Some(i) => i,
                    None => {
                        anyhow::bail!("IP address is required for lockdown validation over network.");
                    }
                };
                
                let stream = tokio::net::TcpStream::connect(SocketAddr::new(target_ip, 62078)).await
                    .context("Failed to connect to device over network")?;
                let mut lc = LockdownClient::new(Idevice::new(Box::new(stream), "idevice_pair"));
                lc.start_session(&pairing_file).await.context("Validation failed")?;
                println!("Lockdown Pairing is VALID.");
            }
        }
        Commands::ListApps { udid } => {
            let dev = get_device(udid).await?;
            let provider = dev.to_provider(UsbmuxdAddr::default(), "idevice_pair");
            let mut ic = InstallationProxyClient::connect(&provider).await?;
            let apps = ic.get_apps(Some("User"), None).await?;
            
            println!("{:<50} {:<30}", "Bundle ID", "Name");
            for (bid, app) in apps {
                let name = app.as_dictionary()
                    .and_then(|d| d.get("CFBundleDisplayName"))
                    .and_then(|v| v.as_string())
                    .unwrap_or("Unknown");
                println!("{:<50} {:<30}", bid, name);
            }
        }
        Commands::Install { udid, file, bundle_id, name } => {
            let dev = get_device(udid).await?;
            let provider = dev.to_provider(UsbmuxdAddr::default(), "idevice_pair");
            let data = std::fs::read(file).context("Failed to read pairing file")?;
            
            let hc = HouseArrestClient::connect(&provider).await?;
            let mut ac = hc.vend_documents(bundle_id.clone()).await?;
            let mut f = ac.open(format!("/Documents/{}", name), idevice::afc::opcode::AfcFopenMode::Wr).await?;
            f.write_all(&data).await.context("Failed to write file to device")?;
            println!("Successfully installed pairing file to {}/Documents/{}", bundle_id, name);
        }
        Commands::Mount { udid } => {
            let dev = get_device(udid).await?;
            println!("Mounting DDI to device {}...", dev.udid);
            mount::auto_mount(dev).await.context("Failed to mount DDI")?;
            println!("DDI mounted successfully.");
        }
    }

    Ok(())
}

async fn connect_to_usbmuxd() -> Result<UsbmuxdConnection> {
    let socket_paths = [
        "/var/run/usbmuxd",
        "/data/data/com.termux/files/usr/var/run/usbmuxd",
    ];

    for path_str in &socket_paths {
        let path = Path::new(path_str);
        if path.exists() {
            println!("Attempting to connect to usbmuxd at: {}", path_str);
            let addr = UsbmuxdAddr::UnixSocket(path_str.to_string());
            match addr.connect(0).await {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    eprintln!("Failed to connect to usbmuxd at {}: {}", path_str, e);
                }
            }
        }
    }

    Err(anyhow::anyhow!(
        "Failed to connect to usbmuxd. Please ensure usbmuxd is running.\n\n
        You might need to start it manually, e.g., by running 'sudo usbmuxd' or 'usbmuxd' in Termux."
    ))
}

async fn get_all_devices() -> Result<Vec<UsbmuxdDevice>> {
    let mut uc = connect_to_usbmuxd().await?;
    let devs = uc.get_devices().await.context("Failed to get devices")?;
    Ok(devs.into_iter().filter(|x| x.connection_type == Connection::Usb).collect())
}

async fn get_device(udid: Option<String>) -> Result<UsbmuxdDevice> {
    let devs = get_all_devices().await?;
    if let Some(target_udid) = udid {
        let udid_upper = target_udid.to_uppercase();
        devs.into_iter().find(|d| d.udid.to_uppercase() == udid_upper)
            .context(format!("Device with UDID {} not found", target_udid))
    } else {
        devs.into_iter().next().context("No iOS devices found via USB")
    }
}

fn pairing_hostname() -> String {
    let suffix: String = uuid::Uuid::new_v4().simple().to_string().chars().take(6).collect();
    format!("idevice_pair-cli-{suffix}")
}

async fn generate_remote_pairing_file(
    provider: &dyn IdeviceProvider,
    hostname: &str,
) -> Result<RpPairingFile> {
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = proxy.create_software_tunnel()?;
    let mut adapter = adapter.to_async_handle();

    let rsd_stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(rsd_stream).await?;
    let tunnel_service = handshake.services.get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
        .context("Untrusted tunnel service not found")?;

    let ts_stream = adapter.connect(tunnel_service.port).await?;
    let mut remote_xpc = RemoteXpcClient::new(ts_stream).await?;
    remote_xpc.do_handshake().await?;
    let _ = remote_xpc.recv_root().await;

    let mut pairing_file = RpPairingFile::generate(hostname);
    let mut pairing_client = RemotePairingClient::new(remote_xpc, hostname);
    pairing_client.connect(&mut pairing_file, || async { "000000".to_string() }).await?;

    Ok(pairing_file)
}

async fn validate_remote(provider: &dyn IdeviceProvider, pairing_file: &mut RpPairingFile) -> Result<()> {
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = proxy.create_software_tunnel()?;
    let mut adapter = adapter.to_async_handle();

    let rsd_stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(rsd_stream).await?;
    let tunnel_service = handshake.services.get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
        .context("Untrusted tunnel service not found")?;

    let ts_stream = adapter.connect(tunnel_service.port).await?;
    let mut conn = RemoteXpcClient::new(ts_stream).await?;
    conn.do_handshake().await?;
    let _ = conn.recv_root().await;

    let hostname = pairing_hostname();
    let mut rpc = RemotePairingClient::new(conn, &hostname);
    let _ = rpc.attempt_pair_verify().await?;
    rpc.validate_pairing(pairing_file).await.context("Validation failed")?;
    Ok(())
}

fn save_pairing_data(data: Vec<u8>, output: Option<PathBuf>, default_name: &str) -> Result<()> {
    let path = output.unwrap_or_else(|| PathBuf::from(default_name));
    std::fs::write(&path, data).context(format!("Failed to write pairing file to {:?}", path))?;
    println!("Pairing file saved to: {:?}", path);
    Ok(())
}
