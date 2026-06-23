use anyhow::Result;
use clap::{Parser, Subcommand};
use idevice::{
    core_device_proxy::CoreDeviceProxy,
    house_arrest::HouseArrestClient,
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    pairing_file::PairingFile,
    provider::IdeviceProvider,
    remote_pairing::{RemotePairingClient, RpPairingFile},
    rsd::RsdHandshake,
    usbmuxd::{UsbmuxdAddr, UsbmuxdDevice},
    Idevice, IdeviceService, RemoteXpcClient,
};
use std::path::PathBuf;
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
    #[command(alias = "l")]
    List,
    /// Pair with a device (Lockdown or Remote)
    #[command(alias = "p")]
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
        /// UDID of the device (if not provided, uses the first one found)
        #[arg(short, long)]
        udid: Option<String>,
    },
    /// List installed apps on the device
    ListApps {
        /// UDID of the device (if not provided, uses the first one found)
        #[arg(short, long)]
        udid: Option<String>,
    },
    /// Install a pairing file to a specific app's Documents folder
    Install {
        /// UDID of the device (if not provided, uses the first one found)
        #[arg(short, long)]
        udid: Option<String>,
        /// Path to the pairing file
        #[arg(short, long)]
        path: PathBuf,
        /// Bundle ID of the app
        #[arg(short, long)]
        bundle_id: String,
        /// Filename in the app's Documents folder
        #[arg(short, long)]
        name: String,
    },
    /// Mount Developer Disk Image (DDI)
    #[command(alias = "m")]
    Mount {
        /// UDID of the device (if not provided, uses the first one found)
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
                println!("No devices found");
                return Ok(());
            }
            println!("Connected devices:");
            for device in devices {
                println!("- {}", device.udid);
            }
        }
        Commands::Pair {
            udid,
            remote,
            output,
        } => {
            let device = get_device(udid).await?;
            if remote {
                let mut rsd = RsdHandshake::new(device.udid.clone()).await?;
                rsd.handshake().await?;
                let mut xpc = RemoteXpcClient::new(rsd).await?;
                let mut client = RemotePairingClient::new(&mut xpc).await?;
                let pairing_file = client.get_pairing_file().await?;
                let output = output.unwrap_or_else(|| PathBuf::from(RP_PAIRING_FILE_NAME));
                pairing_file.save(&output)?;
                println!("Pairing file saved to {:?}", output);
            } else {
                let mut lockdown = LockdownClient::new(device).await?;
                lockdown.pair().await?;
                println!("Successfully paired with device");
            }
        }
        Commands::Validate { udid } => {
            let device = get_device(udid).await?;
            let mut lockdown = LockdownClient::new(device).await?;
            lockdown.validate_pairing().await?;
            println!("Pairing record is valid");
        }
        Commands::ListApps { udid } => {
            let device = get_device(udid).await?;
            let mut lockdown = LockdownClient::new(device).await?;
            let service = lockdown
                .start_service("com.apple.mobile.installation_proxy")
                .await?;
            let mut client = InstallationProxyClient::new(service).await?;
            let apps = client.browse_apps().await?;
            println!("Installed apps:");
            for app in apps {
                println!("- {} ({})", app.bundle_identifier, app.bundle_name);
            }
        }
        Commands::Install {
            udid,
            path,
            bundle_id,
            name,
        } => {
            let device = get_device(udid).await?;
            let mut lockdown = LockdownClient::new(device).await?;
            let service = lockdown.start_service("com.apple.mobile.house_arrest").await?;
            let mut client = HouseArrestClient::new(service).await?;
            client.vend_container(&bundle_id).await?;
            let mut afc = client.start_afc().await?;
            let mut file = tokio::fs::File::open(path).await?;
            let mut contents = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut file, &mut contents).await?;
            let mut remote_file = afc.file_open(&format!("Documents/{}", name), "wb").await?;
            remote_file.write_all(&contents).await?;
            println!("Successfully installed pairing file to {}", bundle_id);
        }
        Commands::Mount { udid } => {
            let device = get_device(udid).await?;
            mount::mount_ddi(device).await?;
            println!("Successfully mounted DDI");
        }
    }

    Ok(())
}

async fn get_all_devices() -> Result<Vec<UsbmuxdDevice>> {
    // Thử tìm socket usbmuxd ở các vị trí phổ biến trên Android/Termux
    let paths = [
        "/var/run/usbmuxd",
        "/data/data/com.termux/files/usr/var/run/usbmuxd",
    ];

    let mut provider = None;
    for path in paths {
        if std::path::Path::new(path).exists() {
            if let Ok(p) = IdeviceProvider::with_addr(UsbmuxdAddr::Unix(path.into())).await {
                provider = Some(p);
                break;
            }
        }
    }

    let mut provider = match provider {
        Some(p) => p,
        None => {
            match IdeviceProvider::new().await {
                Ok(p) => p,
                Err(e) => return Err(anyhow::anyhow!("Failed to connect to usbmuxd. Please ensure 'sudo usbmuxd' is running. Error: {}", e)),
            }
        }
    };

    let devices = provider.get_devices().await?;
    Ok(devices)
}

async fn get_device(udid: Option<String>) -> Result<UsbmuxdDevice> {
    let devices = get_all_devices().await?;
    if let Some(udid) = udid {
        devices
            .into_iter()
            .find(|d| d.udid == udid)
            .ok_or_else(|| anyhow::anyhow!("Device with UDID {} not found", udid))
    } else {
        devices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No devices found"))
    }
}
