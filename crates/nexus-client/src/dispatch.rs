//! Top-level command → handler dispatch.

use std::io::Write;

use crate::cli::{
    AdminSub, BtSub, Cli, Command, EthSub, GnssSub, IfaceSub, PowerSub, ProfileSub, WifiSub,
};
use crate::commands;
use crate::errors::NexusctlError;
use crate::proxy::{BluetoothListFilter, ManagerOps};

pub async fn dispatch(
    cli: &Cli,
    ops: &dyn ManagerOps,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let format = cli.global.output_format();
    let ctx = cli.global.render_context();
    match &cli.command {
        // No subcommand → one-screen status summary.
        None => commands::status::run(ops, format, &ctx, w).await,

        Some(Command::Status) => commands::status::run(ops, format, &ctx, w).await,

        Some(Command::Iface { sub }) => match sub {
            IfaceSub::List { kind } => {
                commands::iface::list(ops, kind.map(|k| k.as_wire()), format, &ctx, w).await
            }
            IfaceSub::Show { iface } => {
                commands::iface::show(ops, iface, None, format, &ctx, w).await
            }
            IfaceSub::Events { iface: _ } => commands::iface::events_stub(w),
        },

        Some(Command::Eth { sub }) => match sub {
            EthSub::List => commands::iface::list(ops, Some("ethernet"), format, &ctx, w).await,
            EthSub::Show { iface } => {
                commands::iface::show(ops, iface, Some("ethernet"), format, &ctx, w).await
            }
        },

        Some(Command::Wifi { sub }) => match sub {
            WifiSub::List => commands::iface::list(ops, Some("wifi"), format, &ctx, w).await,
            WifiSub::Show { iface } => {
                commands::iface::show_wifi(ops, iface.as_deref(), format, &ctx, w).await
            }
        },

        Some(Command::Bt { sub }) => match sub {
            BtSub::Adapters => commands::bt::adapters(ops, format, &ctx, w).await,
            BtSub::List { paired, connected } => {
                let filter = if *paired {
                    BluetoothListFilter::Paired
                } else if *connected {
                    BluetoothListFilter::Connected
                } else {
                    BluetoothListFilter::All
                };
                commands::bt::list(ops, filter, format, &ctx, w).await
            }
            BtSub::Show { address } => commands::bt::show(ops, address, format, &ctx, w).await,
        },

        Some(Command::Gnss { sub }) => match sub {
            GnssSub::List => commands::iface::list(ops, Some("gnss"), format, &ctx, w).await,
            GnssSub::Show { device } => {
                commands::gnss::show(ops, device.as_deref(), format, &ctx, w).await
            }
            GnssSub::Satellites { device } => {
                commands::gnss::satellites(ops, device.as_deref(), format, &ctx, w).await
            }
        },

        Some(Command::Profile { sub }) => match sub {
            ProfileSub::List { kind } => {
                commands::profile::list(ops, kind.map(|k| k.as_wire()), format, &ctx, w).await
            }
            ProfileSub::Show { reference } => {
                commands::profile::show(ops, reference, format, &ctx, w).await
            }
            ProfileSub::Export { reference } => commands::profile::export(ops, reference, w).await,
        },

        Some(Command::Power { sub }) => match sub {
            PowerSub::Get => commands::power::get(ops, format, &ctx, w).await,
        },

        Some(Command::Admin { sub }) => match sub {
            AdminSub::MasterKeyInfo => commands::admin::master_key_info(ops, format, &ctx, w).await,
        },
    }
}
